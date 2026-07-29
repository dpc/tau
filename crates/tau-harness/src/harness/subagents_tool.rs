//! Harness-owned `agent_start`, `wait`, `cancel`, and `message` tools.
//!
//! Watch turn-state transitions follow
//! `SPEC-agent-watch`.
//! Cross-session delivery and sender authentication follow
//! `SPEC-tau-harness-peer-routing`.

mod wait_tracker;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tau_proto::{
    AgentContextKey, AgentContextValue, AgentId, AgentMessageReceived, AgentMessageSent,
    AgentWatchUpdateCause, AgentWatchesUpdated, CborValue, Event, ToolBackgroundError,
    ToolBackgroundResult, ToolCallId, ToolCallRef, ToolError, ToolName, ToolResult, ToolResultKind,
    ToolType,
};
pub(crate) use wait_tracker::PendingWaitSettlement;
use wait_tracker::{
    WaitReply, WaitReplyKind, WaitRequest, WaitTarget, WaitTracker,
    normalized_wait_timeout_minutes_inner, parse_wait_args, wait_error_reply,
    wait_input_available_reply, wait_interrupted_any_reply, wait_interrupted_reply,
    wait_timeout_args,
};

use crate::error::HarnessError;
use crate::event::{ExternalMessageToolCompletedCommand, HarnessCommand, HarnessEvent};
use crate::harness::{
    AgentMessageRecipientStatus, AgentToolCall, Harness, PendingExternalAgentMessageAuth,
    TerminalSettlement,
};

/// Maximum overdue long-wait occurrences published in one scheduler cycle.
pub(super) const MAX_WORK_WAIT_THRESHOLDS_PER_RUNTIME_CYCLE: usize = 64;

/// Compact delayed materialization for crossings captured before later events.
pub(crate) struct PendingLongWaitNotifications {
    /// Watched sender whose wait crossed the thresholds.
    sender_id: String,
    /// Working epoch at the crossing cut.
    status_epoch: u64,
    /// Watch subscriptions present at the crossing cut.
    recipients: Vec<(String, String)>,
    /// Crossed thresholds awaiting per-recipient publication.
    thresholds: crate::agent::CrossedWaitThresholds,
    /// Threshold currently being materialized across recipients.
    current_threshold: Option<u32>,
    /// Next recipient for the current threshold.
    recipient_index: usize,
}

impl PendingLongWaitNotifications {
    /// Retain matching recipients without rewinding the in-progress threshold.
    ///
    /// Returns whether any materialization remains.
    fn retain_recipients(&mut self, retain: impl Fn(&(String, String)) -> bool + Copy) -> bool {
        let removed_before_cursor = self
            .recipients
            .iter()
            .take(self.recipient_index)
            .filter(|recipient| !retain(recipient))
            .count();
        self.recipients.retain(|recipient| retain(recipient));
        if self.recipients.is_empty() {
            return false;
        }
        self.recipient_index = self.recipient_index.saturating_sub(removed_before_cursor);
        if self.current_threshold.is_some() && self.recipients.len() <= self.recipient_index {
            self.recipient_index = 0;
            self.current_threshold = None;
        }
        self.current_threshold.is_some() || !self.thresholds.is_empty()
    }
}

fn provider_status_attempt(state: &tau_proto::AgentWatchProviderState) -> Option<u32> {
    match state {
        tau_proto::AgentWatchProviderState::Retrying { attempt, .. }
        | tau_proto::AgentWatchProviderState::RecoveringContext { attempt }
        | tau_proto::AgentWatchProviderState::TerminalError { attempt, .. } => Some(*attempt),
        tau_proto::AgentWatchProviderState::Blocked { .. }
        | tau_proto::AgentWatchProviderState::DispatchUncertain { .. } => None,
    }
}

fn provider_status_update_is_stale(
    current: &tau_proto::AgentWatchProviderStatusNotification,
    next: &tau_proto::AgentWatchProviderStatusNotification,
) -> bool {
    if next.turn_generation < current.turn_generation {
        return true;
    }
    if next.turn_generation != current.turn_generation
        || next.agent_prompt_id != current.agent_prompt_id
    {
        return false;
    }
    if matches!(
        (&current.state, &next.state),
        (
            tau_proto::AgentWatchProviderState::TerminalError { .. },
            tau_proto::AgentWatchProviderState::Retrying { .. }
                | tau_proto::AgentWatchProviderState::TerminalError { .. }
        )
    ) {
        return true;
    }
    matches!(
        (
            provider_status_attempt(&current.state),
            provider_status_attempt(&next.state)
        ),
        (Some(current_attempt), Some(next_attempt)) if next_attempt < current_attempt
    )
}

/// Model-visible name of the harness-owned wait tool.
pub(crate) const WAIT_TOOL_NAME: &str = "wait";

/// Returns the normalized effective input-wait timeout in minutes.
///
/// An absent `timeout_minutes` selects background-completion waiting and
/// returns `None`. A positive integer selects activating-input waiting and is
/// clamped to 60 minutes. Zero, negative, non-integer, unknown, or conflicting
/// arguments return an error rather than choosing a wait mode implicitly.
pub fn normalized_wait_timeout_minutes(arguments: &CborValue) -> Result<Option<u64>, String> {
    normalized_wait_timeout_minutes_inner(arguments)
}
#[cfg(test)]
pub(crate) const MESSAGE_TOOL_NAME: &str = "message";
#[derive(Default)]
pub(crate) struct SubagentToolState {
    /// State used by the wait tool to track background completions.
    wait_tracker: WaitTracker,
}

static NEXT_AGENT_MESSAGE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
pub(crate) const EXTERNAL_AGENT_MESSAGE_AUTH_TIMEOUT: Duration = Duration::from_secs(25);
pub(crate) const EXTERNAL_AGENT_MESSAGE_RESULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTBOUND_PEER_IO_JOBS: usize = 16;
const MAX_INBOUND_PEER_AUTH_JOBS: usize = 16;
const MAX_INBOUND_PEER_AUTH_JOBS_PER_CONNECTION: usize = 2;
const MAX_EXTERNAL_AGENT_MESSAGE_BYTES: usize = 64 * 1024;
// Admission contract: SPEC-tau-harness-peer-routing.
const MAX_QUEUED_PEER_INPUTS_PER_AGENT: usize = 32;
const MAX_QUEUED_PEER_BYTES_PER_AGENT: usize = 256 * 1024;
const MAX_ACCEPTED_PEER_INPUTS_PER_MINUTE: usize = 60;
/// Diagnostic `PromptOriginator` query prefix for peer auto-start correlation.
const PEER_AUTO_START_QUERY_PREFIX: &str = "peer-auto-start-";
/// Durable non-inheritable metadata key identifying peer-created endpoints.
pub(crate) const PEER_ENTRYPOINT_AGENT_METADATA_KEY: &str = "tau.peer_entrypoint_endpoint";
const INTER_SESSION_UNAVAILABLE: &str = "target session is unavailable for inter-session messaging";

static PEER_IO_ADMISSION: LazyLock<Mutex<PeerIoAdmission>> =
    LazyLock::new(|| Mutex::new(PeerIoAdmission::default()));
static ACTIVE_RUNTIME_LOOKUPS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[derive(Default)]
struct PeerIoAdmission {
    outbound: usize,
    inbound: usize,
    inbound_by_connection: HashMap<tau_proto::ConnectionId, usize>,
}

enum PeerIoPermitKind {
    Outbound,
    Inbound(tau_proto::ConnectionId),
}

pub(crate) struct PeerIoPermit(PeerIoPermitKind);

struct RuntimeLookupPermit;

impl RuntimeLookupPermit {
    fn try_acquire() -> Option<Self> {
        #[allow(
            deprecated,
            reason = "AtomicUsize::try_update requires Rust 1.95, above the workspace MSRV"
        )]
        ACTIVE_RUNTIME_LOOKUPS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_OUTBOUND_PEER_IO_JOBS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for RuntimeLookupPermit {
    fn drop(&mut self) {
        ACTIVE_RUNTIME_LOOKUPS.fetch_sub(1, Ordering::AcqRel);
    }
}

impl PeerIoPermit {
    fn outbound() -> Option<Self> {
        let mut admission = PEER_IO_ADMISSION
            .lock()
            .expect("peer I/O admission poisoned");
        (admission.outbound < MAX_OUTBOUND_PEER_IO_JOBS).then(|| {
            admission.outbound += 1;
            Self(PeerIoPermitKind::Outbound)
        })
    }

    fn inbound(connection_id: tau_proto::ConnectionId) -> Option<Self> {
        let mut admission = PEER_IO_ADMISSION
            .lock()
            .expect("peer I/O admission poisoned");
        let per_connection = admission
            .inbound_by_connection
            .get(&connection_id)
            .copied()
            .unwrap_or(0);
        if admission.inbound >= MAX_INBOUND_PEER_AUTH_JOBS
            || per_connection >= MAX_INBOUND_PEER_AUTH_JOBS_PER_CONNECTION
        {
            return None;
        }
        admission.inbound += 1;
        *admission
            .inbound_by_connection
            .entry(connection_id.clone())
            .or_default() += 1;
        Some(Self(PeerIoPermitKind::Inbound(connection_id)))
    }
}

impl Drop for PeerIoPermit {
    fn drop(&mut self) {
        let mut admission = PEER_IO_ADMISSION
            .lock()
            .expect("peer I/O admission poisoned");
        match &self.0 {
            PeerIoPermitKind::Outbound => admission.outbound -= 1,
            PeerIoPermitKind::Inbound(connection_id) => {
                admission.inbound -= 1;
                let count = admission
                    .inbound_by_connection
                    .get_mut(connection_id)
                    .expect("inbound peer I/O connection count missing");
                *count -= 1;
                if *count == 0 {
                    admission.inbound_by_connection.remove(connection_id);
                }
            }
        }
    }
}

fn build_agent_message_id(
    sender_id: &AgentId,
    timestamp: tau_proto::UnixMicros,
    sequence: u64,
) -> tau_proto::AgentMessageId {
    tau_proto::AgentMessageId::parse(format!(
        "msg-{}-{}-{}",
        sender_id.as_str(),
        timestamp.get(),
        sequence
    ))
    .expect("Tau-generated agent message id must be valid")
}

fn next_agent_message_id(sender_id: &AgentId) -> tau_proto::AgentMessageId {
    build_agent_message_id(
        sender_id,
        tau_proto::UnixMicros::now(),
        NEXT_AGENT_MESSAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    )
}

impl Harness {
    #[cfg(test)]
    pub(crate) fn register_harness_tools(&mut self) {
        self.install_internal_tool_handlers(vec![std::sync::Arc::new(TestBuiltinTools)]);
    }

    pub(crate) fn publish_delegate_roles_context(&mut self) {
        let mut roles: Vec<_> = crate::model::role_infos(
            &self.provider_model_info,
            &self.available_roles,
            &self.available_models,
        )
        .into_iter()
        .filter(|info| {
            crate::model::model_for_role(
                &self.provider_model_info,
                &self.available_roles,
                &info.name,
            )
            .is_some()
        })
        .map(|info| {
            let description = info
                .role_description
                .as_deref()
                .filter(|description| !description.is_empty())
                .unwrap_or(&info.description);
            serde_json::json!({
                "name": info.name,
                "description": description,
            })
        })
        .collect();
        roles.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let agent_ids: Vec<_> = self
            .agents
            .values()
            .filter_map(|agent| agent.agent_id.clone())
            .collect();
        for agent_id in agent_ids {
            self.agent_context.publish(
                tau_proto::AgentId::parse(agent_id).expect("agent id"),
                AgentContextKey::new("delegate_roles"),
                crate::harness::harness_connection_id().clone(),
                "harness".to_owned(),
                AgentContextValue(serde_json::Value::Array(roles.clone())),
            );
        }
    }

    /// Reset retained wait correlation before dispatching a reused call ID.
    pub(crate) fn record_wait_tool_request(&mut self, call_id: &ToolCallId) {
        if let Some(tool) = self.pending_tools.get(call_id) {
            let Some(owner) = self.wait_owner_for_call(call_id) else {
                return;
            };
            self.subagents.wait_tracker.record_tool_invoke(
                call_id.clone(),
                tool.name.clone(),
                owner,
            );
        }
    }

    /// Retain the exact provider declaration before runtime dispatch.
    pub(crate) fn record_wait_tool_call_ref(&mut self, call_id: ToolCallId, call_ref: ToolCallRef) {
        self.subagents
            .wait_tracker
            .reset_call_ref(call_id, call_ref);
    }

    /// Returns the exact provider declaration for a tracked runtime call.
    pub(crate) fn wait_tool_call_ref(&self, call_id: &ToolCallId) -> Option<ToolCallRef> {
        self.subagents.wait_tracker.call_ref(call_id)
    }

    /// Returns the canonical terminal retained for a completed waitable call.
    pub(crate) fn wait_tool_terminal_observation(
        &self,
        call_id: &ToolCallId,
    ) -> Option<tau_proto::ObservationId> {
        self.subagents.wait_tracker.terminal_observation(call_id)
    }

    /// Reports whether the wait projection tracks one pending tool call.
    #[cfg(test)]
    pub(crate) fn wait_tracks_call_for_test(&self, call_id: &ToolCallId) -> bool {
        self.subagents.wait_tracker.tracks_call(call_id)
    }

    /// Record one tool result with its canonical terminal observation, when
    /// any.
    pub(crate) fn record_wait_tool_result(
        &mut self,
        result: ToolResult,
        terminal: Option<tau_proto::ObservationId>,
    ) {
        let Some(owner) = self.wait_owner_for_call(&result.call_id) else {
            return;
        };
        let replies = self
            .subagents
            .wait_tracker
            .record_tool_result(result, owner, terminal);
        self.synchronize_work_waits();
        self.publish_wait_replies(replies);
    }

    /// Record one tool error with its canonical terminal observation, when any.
    pub(crate) fn record_wait_tool_error(
        &mut self,
        error: ToolError,
        terminal: Option<tau_proto::ObservationId>,
    ) {
        let Some(owner) = self.wait_owner_for_call(&error.call_id) else {
            return;
        };
        let replies = self
            .subagents
            .wait_tracker
            .record_tool_error(error, owner, terminal);
        self.synchronize_work_waits();
        self.publish_wait_replies(replies);
    }

    /// Record one background result with its canonical terminal observation.
    pub(crate) fn record_wait_background_result(
        &mut self,
        result: ToolBackgroundResult,
        terminal: Option<tau_proto::ObservationId>,
    ) {
        let Some(owner) = self.wait_owner_for_call(&result.call_id) else {
            return;
        };
        let replies = self
            .subagents
            .wait_tracker
            .record_background_result(result, owner, terminal);
        self.synchronize_work_waits();
        self.publish_wait_replies(replies);
    }

    /// Record one background error with its canonical terminal observation.
    pub(crate) fn record_wait_background_error(
        &mut self,
        error: ToolBackgroundError,
        terminal: Option<tau_proto::ObservationId>,
    ) {
        let Some(owner) = self.wait_owner_for_call(&error.call_id) else {
            return;
        };
        let replies = self
            .subagents
            .wait_tracker
            .record_background_error(error, owner, terminal);
        self.synchronize_work_waits();
        self.publish_wait_replies(replies);
    }

    /// Retire every wait-tracker entry owned by an unloading agent.
    ///
    /// Returns every retired source or wait call ID for outer tool cleanup.
    pub(crate) fn discard_wait_owner_before_teardown(
        &mut self,
        owner: &AgentId,
    ) -> Vec<ToolCallId> {
        self.discard_wait_owner_before_teardown_at(owner, Instant::now())
    }

    /// Retire all waits and their derived semantic clock at one teardown cut.
    pub(crate) fn discard_wait_owner_before_teardown_at(
        &mut self,
        owner: &AgentId,
        now: Instant,
    ) -> Vec<ToolCallId> {
        let discarded = self.subagents.wait_tracker.discard_owner(owner);
        if let Some(agent) = self.agents.get_mut(owner) {
            agent.work_status.retire_wait_at(now);
        }
        discarded
    }

    fn wait_owner_for_call(&self, call_id: &ToolCallId) -> Option<AgentId> {
        self.tool_agents
            .get(call_id)
            .or_else(|| self.peer_internal_tool_agents.get(call_id))
            .or_else(|| self.background_completion_targets.get(call_id))
            .cloned()
    }

    /// Settle runtime accounting and clear one harness-owned internal call.
    pub(crate) fn finish_harness_owned_tool_tracking(&mut self, call_id: &ToolCallId) {
        // ast-grep-ignore: if-let-some-else
        if let Some(cid) = self.peer_internal_tool_agents.get(call_id).cloned() {
            self.tool_turn.mark_complete(call_id);
            if let Some(agent) = self.agents.get_mut(&cid) {
                agent.tools_in_flight = agent.tools_in_flight.saturating_sub(1);
            }
            self.emit_agent_stats_updated(&cid);
        } else {
            self.on_tool_call_complete(call_id.as_str());
        }
        self.clear_tool_call_tracking(call_id.as_str());
    }

    /// Return transcript ownership only for ordinary agent-owned internal
    /// calls with an open durable tool-call node.
    ///
    /// Peer-internal correlation is runtime state and must not graft terminal
    /// facts into a transcript without a matching tool-call node.
    fn harness_owned_terminal_transcript_owner<'a>(
        &self,
        cid: &'a AgentId,
        call_id: &ToolCallId,
    ) -> Option<&'a AgentId> {
        (!self.peer_internal_tool_agents.contains_key(call_id)
            && self.tool_terminal_has_open_durable_owner(cid, call_id))
        .then_some(cid)
    }

    /// Complete waits owned by `owner` after inference-activating input has
    /// been accepted and queued for that same agent.
    pub(crate) fn activate_waits_for(
        &mut self,
        owner: &AgentId,
        activation: tau_proto::ObservationId,
    ) {
        let replies = self
            .subagents
            .wait_tracker
            .activate_waits_for(owner, activation);
        self.synchronize_work_waits();
        self.publish_wait_replies(replies);
    }

    /// Drop runtime-only input-wait registration when its owning agent endpoint
    /// is unloaded.
    pub(crate) fn discard_input_wait_for(&mut self, owner: &AgentId) {
        self.subagents.wait_tracker.discard_input_wait_for(owner);
        if let Some(agent) = self.agents.get_mut(owner) {
            agent.work_status.retire_wait_at(Instant::now());
        }
    }

    /// Return the earliest monotonic deadline among registered input waiters.
    pub(crate) fn next_input_wait_deadline(&self) -> Option<Instant> {
        self.subagents.wait_tracker.next_input_wait_deadline()
    }

    /// Synchronize semantic wait accounting against current tracker ownership.
    pub(crate) fn synchronize_work_waits(&mut self) {
        self.synchronize_work_waits_at(Instant::now());
    }

    /// Synchronize semantic wait accounting at a deterministic monotonic time.
    pub(crate) fn synchronize_work_waits_at(&mut self, now: Instant) {
        self.synchronize_work_wait_clocks_at(now);
        self.capture_crossed_work_wait_thresholds_at(now);
        self.drain_pending_long_wait_notifications_with_budget(
            MAX_WORK_WAIT_THRESHOLDS_PER_RUNTIME_CYCLE,
        );
    }

    /// Synchronize semantic wait clocks without publishing threshold crossings.
    fn synchronize_work_wait_clocks_at(&mut self, now: Instant) {
        let installed = self.subagents.wait_tracker.installed_wait_owners();
        for (cid, agent) in &mut self.agents {
            agent
                .work_status
                .synchronize_wait_at(installed.contains(cid), now);
        }
    }

    /// Return the earliest semantic wait threshold deadline.
    pub(crate) fn next_work_wait_threshold_deadline(&self) -> Option<Instant> {
        self.agents
            .values()
            .filter_map(|agent| agent.work_status.next_wait_deadline())
            .min()
    }

    /// Process a bounded number of semantic wait thresholds due by `now`.
    pub(crate) fn process_work_wait_threshold_deadlines(
        &mut self,
        now: Instant,
        budget: usize,
    ) -> usize {
        self.synchronize_work_wait_clocks_at(now);
        self.capture_crossed_work_wait_thresholds_at(now);
        self.drain_pending_long_wait_notifications_with_budget(budget)
    }

    /// Capture all overdue cursors with the subscriptions present at `now`.
    fn capture_crossed_work_wait_thresholds_at(&mut self, now: Instant) {
        let mut captured = Vec::new();
        for agent in self.agents.values_mut() {
            let Some(thresholds) = agent.work_status.take_all_crossed_wait_thresholds_at(now)
            else {
                continue;
            };
            let Some(sender_id) = agent.agent_id.clone() else {
                continue;
            };
            captured.push((sender_id, agent.work_status.epoch(), thresholds));
        }
        captured.sort_by(|left, right| left.0.cmp(&right.0));
        for (sender_id, status_epoch, thresholds) in captured {
            let recipients = self
                .watchers_for_agent(&sender_id)
                .into_iter()
                .filter_map(|watcher_id| {
                    let subscription_id = self
                        .agent_watch_subscriptions
                        .get(&(watcher_id.clone(), sender_id.clone()))?
                        .clone();
                    Some((watcher_id, subscription_id))
                })
                .collect::<Vec<_>>();
            if recipients.is_empty() {
                continue;
            }
            self.pending_long_wait_notifications
                .push_back(PendingLongWaitNotifications {
                    sender_id,
                    status_epoch,
                    recipients,
                    thresholds,
                    current_threshold: None,
                    recipient_index: 0,
                });
        }
    }

    /// Materialize up to `budget` captured recipient occurrences.
    fn drain_pending_long_wait_notifications(&mut self, budget: usize) -> usize {
        let mut processed = 0;
        while processed < budget {
            let Some(batch) = self.pending_long_wait_notifications.front_mut() else {
                break;
            };
            if batch.current_threshold.is_none() {
                batch.current_threshold = batch.thresholds.pop_next();
            }
            let Some(threshold_minutes) = batch.current_threshold else {
                self.pending_long_wait_notifications.pop_front();
                continue;
            };
            let sender_id = batch.sender_id.clone();
            let status_epoch = batch.status_epoch;
            let (watcher_id, subscription_id) = batch.recipients[batch.recipient_index].clone();
            batch.recipient_index += 1;
            if batch.recipients.len() <= batch.recipient_index {
                batch.recipient_index = 0;
                batch.current_threshold = None;
            }
            let batch_complete = batch.current_threshold.is_none() && batch.thresholds.is_empty();
            self.notify_agent_watcher_about_long_wait(
                &sender_id,
                &watcher_id,
                subscription_id,
                status_epoch,
                threshold_minutes,
            );
            processed += 1;
            if batch_complete {
                self.pending_long_wait_notifications.pop_front();
            }
        }
        processed
    }

    /// Drain against both the caller's limit and the active scheduler budget.
    fn drain_pending_long_wait_notifications_with_budget(&mut self, budget: usize) -> usize {
        let previous_budget = self.long_wait_materialization_budget;
        let effective = previous_budget.map_or(budget, |remaining| remaining.min(budget));
        // Publication re-enters wait synchronization through post-commit tool
        // settlement. Reserve this entire batch before publishing so nested
        // paths cannot recursively acquire a fresh budget.
        self.long_wait_materialization_budget = Some(0);
        let processed = self.drain_pending_long_wait_notifications(effective);
        self.long_wait_materialization_budget =
            previous_budget.map(|remaining| remaining.saturating_sub(processed));
        processed
    }

    /// Return whether captured long-wait occurrences still await
    /// materialization.
    pub(crate) fn has_pending_long_wait_notifications(&self) -> bool {
        !self.pending_long_wait_notifications.is_empty()
    }

    /// Return the front backlog cursor and recipients for deterministic tests.
    #[cfg(test)]
    pub(crate) fn pending_long_wait_front_for_test(
        &self,
    ) -> Option<(usize, Vec<(String, String)>)> {
        self.pending_long_wait_notifications
            .front()
            .map(|batch| (batch.recipient_index, batch.recipients.clone()))
    }

    /// Drain captured notifications against the active runtime-cycle budget.
    pub(crate) fn drain_pending_long_wait_notifications_for_scheduler(&mut self) -> usize {
        self.drain_pending_long_wait_notifications_with_budget(
            MAX_WORK_WAIT_THRESHOLDS_PER_RUNTIME_CYCLE,
        )
    }

    /// Complete every input waiter due at or before `now`.
    pub(crate) fn process_input_wait_deadlines(&mut self, now: Instant) {
        let replies = self.subagents.wait_tracker.expire_input_waits(now);
        self.synchronize_work_waits_at(now);
        self.publish_wait_replies(replies);
    }

    /// Atomically claim an installed harness-owned wait for manual compaction.
    pub(crate) fn claim_wait_for_manual_compaction(
        &mut self,
        owner: &AgentId,
        call_id: &ToolCallId,
    ) -> bool {
        let claimed = self
            .subagents
            .wait_tracker
            .claim_wait_for_manual_compaction(owner, call_id);
        self.synchronize_work_waits();
        claimed
    }

    /// Restore a claimed wait after its canonical cancellation failed to
    /// append.
    pub(crate) fn rollback_manual_compaction_wait_claim(
        &mut self,
        owner: &AgentId,
        call_id: &ToolCallId,
    ) {
        let replies = self
            .subagents
            .wait_tracker
            .rollback_manual_compaction_claim(owner, call_id);
        self.synchronize_work_waits();
        self.publish_wait_replies(replies);
    }

    /// Report whether the named wait is already claimed for manual compaction.
    pub(crate) fn wait_claimed_for_manual_compaction(
        &self,
        owner: &AgentId,
        call_id: &ToolCallId,
    ) -> bool {
        self.subagents
            .wait_tracker
            .wait_claimed_for_manual_compaction(owner, call_id)
    }

    #[cfg(test)]
    /// Reports whether one agent owns an installed input waiter.
    pub(crate) fn input_wait_pending_for(&self, owner: &AgentId) -> bool {
        self.subagents.wait_tracker.input_wait_pending_for(owner)
    }

    #[cfg(test)]
    /// Report whether a placeholder has moved this call into waitable
    /// background state.
    pub(crate) fn wait_call_is_backgrounded_for_test(&self, call_id: &ToolCallId) -> bool {
        self.subagents.wait_tracker.is_backgrounded(call_id)
    }

    /// Record cancellation and optionally correlate one canonical terminal.
    pub(crate) fn record_wait_tool_cancelled(
        &mut self,
        call_ids: &HashSet<ToolCallId>,
        terminal: Option<(&ToolCallId, tau_proto::ObservationId)>,
    ) {
        let cancelled = self
            .subagents
            .wait_tracker
            .record_tool_cancelled(call_ids, terminal);
        self.synchronize_work_waits();
        for call_id in cancelled.unsuppress_call_ids {
            self.unsuppress_background_completion_prompt(call_id);
        }
        for call_id in cancelled.suppress_call_ids {
            self.suppress_background_completion_prompt(call_id);
        }
        self.publish_wait_replies(cancelled.replies);
        if let Some((terminal_call_id, wait_terminal)) = terminal {
            for wait in cancelled
                .cancelled_waits
                .into_iter()
                .filter(|wait| &wait.call_id == terminal_call_id)
            {
                if let (Some(wait_call), Some(wait_observation)) =
                    (wait.call_ref, wait.wait_observation)
                {
                    self.append_best_effort_observation(
                        &wait.owner,
                        tau_proto::ObservationId::random(),
                        Event::AgentToolWaitSettled(tau_proto::AgentToolWaitSettled {
                            wait_observation,
                            wait_call,
                            registration: wait.registration,
                            wait_terminal,
                            outcome: tau_proto::ToolWaitOutcome::Cancelled,
                        }),
                    );
                }
            }
        }
    }

    /// Handle the harness-owned `message` tool call inline.
    /// Publish an agent message after validating sender and recipient state.
    pub(crate) fn publish_agent_message_from_agent(
        &mut self,
        agent_id: &AgentId,
        recipient_id: String,
        message: String,
    ) -> Result<(), String> {
        self.publish_agent_delivery_from_agent(
            agent_id,
            recipient_id,
            message,
            tau_proto::AgentMessageKind::Message,
        )
    }

    /// Resolve and publish one current-session bare entrypoint message.
    pub(crate) fn publish_peer_entrypoint_message_from_agent(
        &mut self,
        conversation_id: &AgentId,
        message: String,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
    ) -> Result<(), String> {
        let sender = self
            .ensure_agent_id_for_agent(conversation_id)
            .ok_or_else(|| "sender agent no longer exists".to_owned())?;
        let sender_id = crate::parse_agent_id(&sender);
        if message.len() > MAX_EXTERNAL_AGENT_MESSAGE_BYTES {
            return Err("peer message exceeds the 64 KiB limit".to_owned());
        }
        let message_id = next_agent_message_id(&sender_id);
        if self.pending_external_receive_acks.len() >= MAX_INBOUND_PEER_AUTH_JOBS {
            return Err("peer receive commit queue is busy; retry later".to_owned());
        }
        if self.pending_external_receive_acks.contains_key(&message_id) {
            return Err("peer receive is already pending".to_owned());
        }
        let (recipient_id, started, rate_admitted_at) =
            self.resolve_peer_entrypoint_recipient(&message_id, message.len())?;
        let received = AgentMessageReceived {
            message_id: message_id.clone(),
            sender_id: sender_id.clone(),
            sender_session_id: None,
            recipient_id: recipient_id.clone(),
            kind: tau_proto::AgentMessageKind::Message,
            watch_turn_state: None,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            message: message.clone(),
        };
        self.pending_external_receive_acks.insert(
            message_id.clone(),
            crate::harness::PendingExternalReceiveAck {
                session_generation: self.current_session_generation,
                recipient_id: recipient_id.clone(),
                recipient: tau_proto::ExternalAgentMessageRecipient::BareEntrypoint,
                expected_receive: received.clone(),
                canceled: false,
                started,
                reselect_attempted: false,
                rate_admitted_at,
                completion: crate::harness::PendingPeerReceiveCompletion::Local {
                    conversation_id: conversation_id.clone(),
                    call_id,
                    tool_name,
                    tool_type,
                    sender_id: sender_id.clone(),
                    message: message.clone(),
                },
            },
        );
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::AgentMessageReceived(received),
        );
        Ok(())
    }

    /// Publish final-response watch content only while the watched sender
    /// remains a live agent endpoint.
    pub(crate) fn publish_agent_watch_response_from_agent(
        &mut self,
        agent_id: &AgentId,
        recipient_id: String,
        message: String,
    ) -> Result<(), String> {
        let sender_id = self
            .ensure_agent_id_for_agent(agent_id)
            .ok_or_else(|| "watched agent no longer exists".to_owned())?;
        if self.agent_message_recipient_status(&sender_id) != AgentMessageRecipientStatus::Live {
            return Err("watched agent is no longer live".to_owned());
        }
        self.publish_agent_delivery_from_agent(
            agent_id,
            recipient_id,
            message,
            tau_proto::AgentMessageKind::WatchResponse,
        )
    }

    /// Validate and apply one session-local watch relation at the authoritative
    /// production boundary.
    ///
    /// Self-watch is rejected first. Enable then classifies the target as Live,
    /// Stopped, or Unknown before checking only genuinely new edges for a
    /// closing path. Re-enabling an existing edge preserves snapshot behavior;
    /// disable bypasses lifecycle and cycle analysis. Every failure precedes
    /// mutation and event publication.
    ///
    /// See `SPEC-agent-watch` and
    /// `GATE-agent-watch-acyclic-topology`.
    pub(crate) fn try_set_agent_watch(
        &mut self,
        watcher_id: &str,
        watched_agent_id: &str,
        enable: bool,
        cause: AgentWatchUpdateCause,
    ) -> Result<(), String> {
        if watcher_id == watched_agent_id {
            return Err("`agent_id` must identify another agent".to_owned());
        }
        if enable {
            match self.agent_message_recipient_status(watched_agent_id) {
                AgentMessageRecipientStatus::Live => {}
                AgentMessageRecipientStatus::Stopped => {
                    return Err(format!("agent is not live: `{watched_agent_id}`"));
                }
                AgentMessageRecipientStatus::Unknown => {
                    return Err(format!("unknown agent: `{watched_agent_id}`"));
                }
            }
            let already_present = self
                .agent_watches
                .get(watcher_id)
                .is_some_and(|watched| watched.contains(watched_agent_id));
            if !already_present && self.agent_watch_path_exists(watched_agent_id, watcher_id) {
                return Err(format!(
                    "agent watch would create a cycle: `{watcher_id}` -> `{watched_agent_id}`"
                ));
            }
        }
        self.apply_agent_watch(watcher_id, watched_agent_id, enable, cause);
        Ok(())
    }

    /// Return whether `target_id` is reachable from `start_id` through forward
    /// watch edges.
    ///
    /// Iterative traversal avoids call-stack growth, while visitation also
    /// terminates on deliberately malformed test or invariant-violating
    /// topology.
    pub(crate) fn agent_watch_path_exists(&self, start_id: &str, target_id: &str) -> bool {
        let mut pending = vec![start_id.to_owned()];
        let mut visited = HashSet::new();

        while let Some(agent_id) = pending.pop() {
            if agent_id == target_id {
                return true;
            }
            if !visited.insert(agent_id.clone()) {
                continue;
            }
            if let Some(watched) = self.agent_watches.get(&agent_id) {
                pending.extend(watched.iter().cloned());
            }
        }
        false
    }

    /// Apply one already-validated session-local watch relation and publish the
    /// authoritative watcher snapshot.
    ///
    /// Production enables reach this helper only from
    /// [`Self::try_set_agent_watch`]. Internal cleanup may call it directly
    /// only for disable.
    fn apply_agent_watch(
        &mut self,
        watcher_id: &str,
        watched_agent_id: &str,
        enable: bool,
        cause: AgentWatchUpdateCause,
    ) {
        if watcher_id == watched_agent_id {
            return;
        }
        if enable {
            let inserted = self
                .agent_watches
                .entry(watcher_id.to_owned())
                .or_default()
                .insert(watched_agent_id.to_owned());
            self.agent_watchers
                .entry(watched_agent_id.to_owned())
                .or_default()
                .insert(watcher_id.to_owned());
            if inserted {
                let subscription_id = format!(
                    "watch-{}",
                    next_agent_message_id(&crate::parse_agent_id(watcher_id)).as_str()
                );
                self.agent_watch_subscriptions.insert(
                    (watcher_id.to_owned(), watched_agent_id.to_owned()),
                    subscription_id,
                );
            }
            self.publish_agent_watches_snapshot(watcher_id, Some(watched_agent_id), cause);
            self.notify_agent_watcher_work_status(watcher_id, watched_agent_id, true);
            if let Some(status) = self
                .agent_watch_provider_status
                .get(watched_agent_id)
                .cloned()
            {
                self.notify_agent_watcher_provider_status(
                    watcher_id,
                    watched_agent_id,
                    &status,
                    true,
                );
            }
            return;
        } else {
            if let Some(watched) = self.agent_watches.get_mut(watcher_id) {
                watched.remove(watched_agent_id);
                if watched.is_empty() {
                    self.agent_watches.remove(watcher_id);
                }
            }
            if let Some(watchers) = self.agent_watchers.get_mut(watched_agent_id) {
                watchers.remove(watcher_id);
                if watchers.is_empty() {
                    self.agent_watchers.remove(watched_agent_id);
                }
            }
            self.retire_agent_watch_subscription(watcher_id, watched_agent_id);
        }
        self.publish_agent_watches_snapshot(watcher_id, Some(watched_agent_id), cause);
    }

    /// Mutate watch topology without validation for focused harness fixtures.
    ///
    /// Production callers must use [`Self::try_set_agent_watch`], except for
    /// internal disable-only cleanup through [`Self::apply_agent_watch`].
    #[cfg(test)]
    pub(crate) fn set_agent_watch(
        &mut self,
        watcher_id: &str,
        watched_agent_id: &str,
        enable: bool,
        cause: AgentWatchUpdateCause,
    ) {
        self.apply_agent_watch(watcher_id, watched_agent_id, enable, cause);
    }

    /// Return a sorted snapshot of current watcher ids for a watched agent.
    pub(crate) fn watchers_for_agent(&self, watched_agent_id: &str) -> Vec<String> {
        self.agent_watchers
            .get(watched_agent_id)
            .map(|watchers| watchers.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Return a safe one-line current provider snapshot for an `agent_watch`
    /// tool result. Historical attempts and provider-authored text are
    /// excluded.
    pub(crate) fn agent_watch_provider_status_summary(
        &self,
        watched_agent_id: &str,
    ) -> Option<String> {
        self.agent_watch_provider_status
            .get(watched_agent_id)
            .map(|status| crate::prompt::watch_provider_status_summary(&status.state))
    }

    /// Remove a stale watch relation and publish the updated watcher snapshot.
    pub(crate) fn prune_agent_watch(&mut self, watcher_id: &str, watched_agent_id: &str) {
        self.apply_agent_watch(
            watcher_id,
            watched_agent_id,
            false,
            AgentWatchUpdateCause::WatcherPruned,
        );
    }

    /// Retire every watch relation and provider snapshot involving an unloaded
    /// endpoint.
    ///
    /// This operation is idempotent because both the durable unload reaction
    /// and the local removal fallback may observe the same endpoint.
    /// Surviving watchers receive an authoritative replacement snapshot;
    /// the unloaded watcher does not receive another event addressed to it.
    ///
    /// See `SPEC-agent-watch`.
    pub(crate) fn retire_agent_watch_endpoint(&mut self, agent_id: &str) {
        self.pending_long_wait_notifications.retain_mut(|batch| {
            if batch.sender_id == agent_id {
                return false;
            }
            batch.retain_recipients(|(watcher_id, _)| watcher_id != agent_id)
        });
        let outgoing = self.agent_watches.remove(agent_id).unwrap_or_default();
        let incoming = self.agent_watchers.remove(agent_id).unwrap_or_default();

        for watched_agent_id in outgoing {
            if let Some(watchers) = self.agent_watchers.get_mut(&watched_agent_id) {
                watchers.remove(agent_id);
                if watchers.is_empty() {
                    self.agent_watchers.remove(&watched_agent_id);
                }
            }
            self.retire_agent_watch_subscription(agent_id, &watched_agent_id);
        }

        for watcher_id in incoming {
            if let Some(watched) = self.agent_watches.get_mut(&watcher_id) {
                watched.remove(agent_id);
                if watched.is_empty() {
                    self.agent_watches.remove(&watcher_id);
                }
            }
            self.retire_agent_watch_subscription(&watcher_id, agent_id);
            if self.agent_message_recipient_status(&watcher_id) == AgentMessageRecipientStatus::Live
            {
                self.publish_agent_watches_snapshot(
                    &watcher_id,
                    Some(agent_id),
                    AgentWatchUpdateCause::WatcherPruned,
                );
            }
        }

        self.agent_watch_provider_status.remove(agent_id);
    }

    /// Remove one subscription identity and all delivery-dedupe state it owns.
    fn retire_agent_watch_subscription(&mut self, watcher_id: &str, watched_agent_id: &str) {
        if let Some(subscription_id) = self
            .agent_watch_subscriptions
            .remove(&(watcher_id.to_owned(), watched_agent_id.to_owned()))
        {
            self.pending_long_wait_notifications.retain_mut(|batch| {
                batch.retain_recipients(|(_, candidate)| candidate != &subscription_id)
            });
            self.agent_watch_provider_deliveries
                .remove(&subscription_id);
        }
    }

    fn publish_agent_watches_snapshot(
        &mut self,
        watcher_id: &str,
        changed_agent_id: Option<&str>,
        cause: AgentWatchUpdateCause,
    ) {
        let watched_agent_ids = self
            .agent_watches
            .get(watcher_id)
            .map(|watched| {
                watched
                    .iter()
                    .map(crate::parse_agent_id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::AgentWatchesUpdated(AgentWatchesUpdated {
                session_id: self.current_session_id.clone(),
                watcher_id: crate::parse_agent_id(watcher_id),
                watched_agent_ids,
                changed_agent_id: changed_agent_id.map(crate::parse_agent_id),
                cause,
            }),
        );
    }

    /// Apply one canonical report and fan out its typed live transition.
    pub(crate) fn report_agent_work_status(
        &mut self,
        conversation_id: &AgentId,
        report: crate::WorkStatusReport,
    ) -> Result<bool, String> {
        let now = Instant::now();
        self.synchronize_work_waits_at(now);
        let wait_installed = self
            .subagents
            .wait_tracker
            .installed_wait_owners()
            .contains(conversation_id);
        let (changed, watched_agent_id) = {
            let Some(agent) = self.agents.get_mut(conversation_id) else {
                return Err("status caller is no longer loaded".to_owned());
            };
            let current = &mut agent.work_status;
            if !current.report_at(report, now, wait_installed) {
                return Ok(false);
            }
            (true, agent.agent_id.clone())
        };
        if let Some(watched_agent_id) = watched_agent_id {
            for watcher_id in self.watchers_for_agent(&watched_agent_id) {
                self.notify_agent_watcher_work_status(&watcher_id, &watched_agent_id, false);
            }
        }
        Ok(changed)
    }

    /// Deliver one typed current or live work-status projection.
    pub(crate) fn notify_agent_watcher_work_status(
        &mut self,
        watcher_id: &str,
        watched_agent_id: &str,
        initial: bool,
    ) {
        if self.agent_message_recipient_status(watcher_id) != AgentMessageRecipientStatus::Live {
            return;
        }
        let Some(subscription_id) = self
            .agent_watch_subscriptions
            .get(&(watcher_id.to_owned(), watched_agent_id.to_owned()))
            .cloned()
        else {
            return;
        };
        let status = self
            .agent_routes
            .get(watched_agent_id)
            .and_then(|cid| self.agents.get(cid))
            .map(|agent| agent.work_status.clone())
            .unwrap_or_default();
        let sender_id = crate::parse_agent_id(watched_agent_id);
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::AgentMessageReceived(AgentMessageReceived {
                message_id: next_agent_message_id(&sender_id),
                sender_id,
                sender_session_id: None,
                recipient_id: crate::parse_agent_id(watcher_id),
                kind: tau_proto::AgentMessageKind::WatchWorkStatus,
                watch_turn_state: None,
                watch_provider_status: None,
                watch_work_status: Some(tau_proto::AgentWatchWorkStatusNotification {
                    session_id: self.current_session_id.clone(),
                    subscription_id,
                    status_epoch: status.epoch(),
                    phase: status.phase(),
                    title: status.title().map(ToOwned::to_owned),
                    initial,
                }),
                watch_long_wait: None,
                message: String::new(),
            }),
        );
    }

    /// Materialize one captured long-wait occurrence for one subscription.
    fn notify_agent_watcher_about_long_wait(
        &mut self,
        watched_agent_id: &str,
        watcher_id: &str,
        subscription_id: String,
        status_epoch: u64,
        threshold_minutes: u32,
    ) {
        if self.agent_message_recipient_status(watcher_id) != AgentMessageRecipientStatus::Live {
            return;
        }
        let sender_id = crate::parse_agent_id(watched_agent_id);
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::AgentMessageReceived(AgentMessageReceived {
                message_id: next_agent_message_id(&sender_id),
                sender_id,
                sender_session_id: None,
                recipient_id: crate::parse_agent_id(watcher_id),
                kind: tau_proto::AgentMessageKind::WatchLongWait,
                watch_turn_state: None,
                watch_provider_status: None,
                watch_work_status: None,
                watch_long_wait: Some(tau_proto::AgentWatchLongWaitNotification {
                    session_id: self.current_session_id.clone(),
                    subscription_id,
                    status_epoch,
                    threshold_minutes,
                }),
                message: String::new(),
            }),
        );
    }

    /// Record and fan out a sanitized provider snapshot, hard-deduplicated per
    /// subscription, turn, prompt, phase, and category.
    pub(crate) fn update_agent_watch_provider_status(
        &mut self,
        watched_agent_id: &str,
        mut status: tau_proto::AgentWatchProviderStatusNotification,
    ) {
        if self.agent_message_recipient_status(watched_agent_id)
            != AgentMessageRecipientStatus::Live
        {
            return;
        }
        status.initial = false;
        if self
            .agent_watch_provider_status
            .get(watched_agent_id)
            .is_some_and(|current| provider_status_update_is_stale(current, &status))
        {
            return;
        }
        self.agent_watch_provider_status
            .insert(watched_agent_id.to_owned(), status.clone());
        for watcher_id in self.watchers_for_agent(watched_agent_id) {
            self.notify_agent_watcher_provider_status(
                &watcher_id,
                watched_agent_id,
                &status,
                false,
            );
        }
    }

    /// Project harness-owned durable recovery state into the current sanitized
    /// watcher snapshot.
    pub(crate) fn project_agent_watch_provider_state(
        &mut self,
        cid: &crate::AgentId,
        agent_prompt_id: tau_proto::AgentPromptId,
        state: tau_proto::AgentWatchProviderState,
    ) {
        let Some(watched_agent_id) = self.ensure_agent_id_for_agent(cid) else {
            return;
        };
        let turn_generation = self
            .agents
            .get(cid)
            .map_or(0, |agent| agent.turn_generation);
        self.update_agent_watch_provider_status(
            &watched_agent_id,
            tau_proto::AgentWatchProviderStatusNotification {
                session_id: self.current_session_id.clone(),
                subscription_id: String::new(),
                turn_generation,
                agent_prompt_id,
                state,
                initial: false,
            },
        );
    }

    fn notify_agent_watcher_provider_status(
        &mut self,
        watcher_id: &str,
        watched_agent_id: &str,
        status: &tau_proto::AgentWatchProviderStatusNotification,
        initial: bool,
    ) {
        if self.agent_message_recipient_status(watcher_id) != AgentMessageRecipientStatus::Live
            || self.agent_message_recipient_status(watched_agent_id)
                != AgentMessageRecipientStatus::Live
        {
            return;
        }
        let Some(subscription_id) = self
            .agent_watch_subscriptions
            .get(&(watcher_id.to_owned(), watched_agent_id.to_owned()))
            .cloned()
        else {
            return;
        };
        if !initial {
            let deliveries = self
                .agent_watch_provider_deliveries
                .entry(subscription_id.clone())
                .or_default();
            let decision = deliveries.record(
                status.turn_generation,
                &status.agent_prompt_id,
                crate::harness::AgentWatchProviderDeliveryKind::from(&status.state),
            );
            tracing::trace!(
                target: "tau_harness::agent_watch",
                subscription_id,
                turn_generation = status.turn_generation,
                tracked_prompts = deliveries.prompt_count(),
                tracked_delivery_keys = deliveries.delivery_key_count(),
                should_deliver = decision.should_deliver,
                stale_generation = decision.stale_generation,
                capacity_evicted = decision.capacity_evicted,
                terminal_retired = decision.terminal_retired,
                "updated provider-status delivery dedupe cardinality"
            );
            if decision.capacity_evicted {
                tracing::debug!(
                    target: "tau_harness::agent_watch",
                    subscription_id,
                    turn_generation = status.turn_generation,
                    tracked_prompts = deliveries.prompt_count(),
                    tracked_delivery_keys = deliveries.delivery_key_count(),
                    "evicted oldest provider-status prompt from delivery dedupe state"
                );
            }
            if !decision.should_deliver {
                return;
            }
        }
        let mut status = status.clone();
        status.session_id = self.current_session_id.clone();
        status.subscription_id = subscription_id;
        status.initial = initial;
        let sender_id = crate::parse_agent_id(watched_agent_id);
        let message = crate::prompt::watch_provider_status_text(watched_agent_id, &status);
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::AgentMessageReceived(AgentMessageReceived {
                message_id: next_agent_message_id(&sender_id),
                sender_id,
                sender_session_id: None,
                recipient_id: crate::parse_agent_id(watcher_id),
                kind: tau_proto::AgentMessageKind::WatchProviderStatus,
                watch_turn_state: None,
                watch_provider_status: Some(status),
                watch_work_status: None,
                watch_long_wait: None,
                message,
            }),
        );
    }

    pub(crate) fn publish_agent_delivery_from_agent(
        &mut self,
        agent_id: &AgentId,
        recipient_id: String,
        message: String,
        kind: tau_proto::AgentMessageKind,
    ) -> Result<(), String> {
        let sender_id = self
            .ensure_agent_id_for_agent(agent_id)
            .ok_or_else(|| "sender agent no longer exists".to_owned())?;
        if recipient_id != "user" {
            match self.agent_message_recipient_status(&recipient_id) {
                AgentMessageRecipientStatus::Live => {}
                AgentMessageRecipientStatus::Stopped => {
                    return Err(format!("stopped message recipient: `{recipient_id}`"));
                }
                AgentMessageRecipientStatus::Unknown => {
                    return Err(format!("unknown message recipient: `{recipient_id}`"));
                }
            }
        }
        let recipient = if recipient_id == "user" {
            tau_proto::AgentMessageRecipient::User
        } else {
            tau_proto::AgentMessageRecipient::Agent {
                agent_id: crate::parse_agent_id(&recipient_id),
            }
        };
        let sender_id: tau_proto::AgentId = crate::parse_agent_id(&sender_id);
        let message_id = next_agent_message_id(&sender_id);
        if kind == tau_proto::AgentMessageKind::Message {
            self.publish_for_agent_from(
                agent_id,
                Some(crate::harness::harness_connection_id()),
                Event::AgentMessageSent(AgentMessageSent {
                    message_id: message_id.clone(),
                    sender_id: sender_id.clone(),
                    recipient: recipient.clone(),
                    kind,
                    message: message.clone(),
                }),
            );
        }
        if let tau_proto::AgentMessageRecipient::Agent { agent_id } = recipient {
            self.publish_event(
                Some(crate::harness::harness_connection_id()),
                Event::AgentMessageReceived(AgentMessageReceived {
                    message_id,
                    sender_id,
                    sender_session_id: None,
                    recipient_id: agent_id,
                    kind,
                    watch_turn_state: None,
                    watch_provider_status: None,
                    watch_work_status: None,
                    watch_long_wait: None,
                    message,
                }),
            );
        }
        Ok(())
    }

    /// Prepare the sender-side projection for an external message and start a
    /// worker that performs runtime-dir lookup plus socket RPC off the harness
    /// event loop. The projection is published only after confirmed delivery.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn publish_external_agent_message_from_agent(
        &mut self,
        conversation_id: &AgentId,
        recipient_session_id: tau_proto::SessionId,
        recipient: tau_proto::ExternalAgentMessageRecipient,
        message: String,
        kind: tau_proto::AgentMessageKind,
        completion: Option<ExternalMessageToolCompletion>,
    ) -> Result<(), String> {
        let permit = PeerIoPermit::outbound()
            .ok_or_else(|| "peer message I/O is busy; retry later".to_owned())?;
        let sender_id = self
            .ensure_agent_id_for_agent(conversation_id)
            .ok_or_else(|| "sender agent no longer exists".to_owned())?;
        let sender_id: tau_proto::AgentId = crate::parse_agent_id(&sender_id);
        let message_id = next_agent_message_id(&sender_id);
        let request_id = format!("external-{message_id}");
        let capability = random_external_message_capability(&mut self.agent_id_rng);
        let publish_sent = kind == tau_proto::AgentMessageKind::Message;
        self.pending_external_message_auth.insert(
            message_id.clone(),
            PendingExternalAgentMessageAuth {
                capability: capability.clone(),
                sender_session_id: self.current_session_id.clone(),
                sender_id: sender_id.clone(),
                recipient_session_id: recipient_session_id.clone(),
                recipient: recipient.clone(),
                kind,
                message: message.clone(),
            },
        );
        let request = tau_proto::ExternalAgentMessageRequest {
            request_id,
            message_id,
            capability,
            sender_session_id: self.current_session_id.clone(),
            sender_id,
            recipient_session_id,
            recipient,
            kind,
            message,
        };
        let tx = self.tx.clone();
        let cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.peer_io_cancellations
            .retain(|pending| pending.strong_count() > 0);
        self.peer_io_cancellations
            .push(Arc::downgrade(&cancellation));
        thread::spawn(move || {
            let _permit = permit;
            let auth_message_id = request.message_id.clone();
            let result = send_external_agent_message_request(request.clone(), &cancellation);
            if let Some(completion) = completion {
                // This call is intentionally best-effort; preserve the existing discarded
                // result. ast-grep-ignore: let-underscore-call
                let _ = tx.send(HarnessEvent::Command(
                    HarnessCommand::ExternalMessageToolCompleted(Box::new(
                        ExternalMessageToolCompletedCommand {
                            _permit: Some(_permit),
                            conversation_id: completion.conversation_id,
                            session_generation: completion.session_generation,
                            call_id: completion.call_id,
                            tool_name: completion.tool_name,
                            tool_type: completion.tool_type,
                            result,
                            details: completion.details,
                            auth_message_id,
                            publish_sent,
                            sender_id: request.sender_id.clone(),
                            recipient_session_id: request.recipient_session_id.clone(),
                            kind: request.kind,
                            message: request.message.clone(),
                        },
                    )),
                ));
            }
        });
        Ok(())
    }

    /// Authenticate a target-harness callback for a pending outbound external
    /// message.
    pub(crate) fn handle_external_agent_message_auth_request(
        &mut self,
        request: tau_proto::ExternalAgentMessageAuthRequest,
    ) -> tau_proto::ExternalAgentMessageAuthResult {
        let request_id = request.request_id.clone();
        let result = self.authorize_external_agent_message(request);
        tau_proto::ExternalAgentMessageAuthResult {
            request_id,
            authorized: result.is_ok(),
            error: result.err(),
        }
    }

    fn authorize_external_agent_message(
        &self,
        request: tau_proto::ExternalAgentMessageAuthRequest,
    ) -> Result<(), String> {
        let Some(pending) = self.pending_external_message_auth.get(&request.message_id) else {
            return Err("unknown external message capability".to_owned());
        };
        if pending.capability != request.capability
            || pending.sender_session_id != request.sender_session_id
            || pending.sender_id != request.sender_id
            || pending.recipient_session_id != request.recipient_session_id
            || pending.recipient != request.recipient
            || pending.kind != request.kind
            || pending.message != request.message
        {
            return Err("external message capability does not match request".to_owned());
        }
        if request.sender_session_id != self.current_session_id {
            return Err(format!(
                "sender harness is on active session `{}`, not `{}`",
                self.current_session_id, request.sender_session_id
            ));
        }
        Ok(())
    }

    /// Handle an external agent message RPC accepted from a socket client.
    pub(crate) fn start_external_agent_message_auth(
        &mut self,
        client_id: tau_proto::ConnectionId,
        request: tau_proto::ExternalAgentMessageRequest,
    ) -> Option<tau_proto::ExternalAgentMessageResult> {
        let request_id = request.request_id.clone();
        if let Err(error) = self.validate_external_agent_message_syntax(&request) {
            return Some(tau_proto::ExternalAgentMessageResult {
                request_id,
                error: Some(error),
                recipient_id: None,
                started: false,
            });
        }
        let Some(permit) = PeerIoPermit::inbound(client_id.clone()) else {
            return Some(tau_proto::ExternalAgentMessageResult {
                request_id,
                error: Some("peer message authentication is busy; retry later".to_owned()),
                recipient_id: None,
                started: false,
            });
        };
        let tx = self.tx.clone();
        let session_generation = self.current_session_generation;
        let cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancellations = self
            .inbound_peer_io_cancellations
            .entry(client_id.clone())
            .or_default();
        cancellations.retain(|pending| pending.strong_count() > 0);
        cancellations.push(Arc::downgrade(&cancellation));
        thread::spawn(move || {
            let result = authenticate_external_agent_message_sender(&request, &cancellation)
                .map_err(|_| "external message authentication failed".to_owned());
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = tx.send(HarnessEvent::Command(
                HarnessCommand::ExternalMessageAuthCompleted(Box::new(
                    crate::event::ExternalMessageAuthCompletedCommand {
                        _permit: Some(permit),
                        client_id,
                        session_generation,
                        request,
                        result,
                    },
                )),
            ));
        });
        None
    }

    pub(crate) fn complete_external_agent_message_auth(
        &mut self,
        client_id: tau_proto::ConnectionId,
        session_generation: u64,
        request: tau_proto::ExternalAgentMessageRequest,
        result: Result<(), String>,
    ) -> Option<tau_proto::ExternalAgentMessageResult> {
        let request_id = request.request_id.clone();
        if let Err(error) = result {
            return Some(tau_proto::ExternalAgentMessageResult {
                request_id,
                error: Some(error),
                recipient_id: None,
                started: false,
            });
        }
        match self.queue_external_agent_message_receive(client_id, session_generation, request) {
            Ok(()) => None,
            Err(error) => Some(tau_proto::ExternalAgentMessageResult {
                request_id,
                error: Some(error),
                recipient_id: None,
                started: false,
            }),
        }
    }

    fn queue_external_agent_message_receive(
        &mut self,
        client_id: tau_proto::ConnectionId,
        session_generation: u64,
        request: tau_proto::ExternalAgentMessageRequest,
    ) -> Result<(), String> {
        self.validate_external_agent_message_target(&request)?;
        if session_generation != self.current_session_generation {
            return Err("target session changed before peer admission".to_owned());
        }
        if !self.external_message_peers.contains(&client_id) {
            return Err("peer connection closed before peer admission".to_owned());
        }
        if self.pending_external_receive_acks.len() >= MAX_INBOUND_PEER_AUTH_JOBS {
            return Err("peer receive commit queue is busy; retry later".to_owned());
        }
        let message_id = request.message_id.clone();
        if self.pending_external_receive_acks.contains_key(&message_id) {
            return Err("peer receive is already pending".to_owned());
        }
        let (recipient_id, started, rate_admitted_at) = match &request.recipient {
            tau_proto::ExternalAgentMessageRecipient::Exact(agent_id) => {
                let admitted_at = self.admit_peer_input(agent_id, request.message.len())?;
                (agent_id.clone(), false, admitted_at)
            }
            tau_proto::ExternalAgentMessageRecipient::BareEntrypoint => {
                self.resolve_peer_entrypoint_recipient(&request.message_id, request.message.len())?
            }
        };
        let received = AgentMessageReceived {
            message_id: message_id.clone(),
            sender_id: request.sender_id,
            sender_session_id: Some(request.sender_session_id),
            recipient_id: recipient_id.clone(),
            kind: request.kind,
            watch_turn_state: None,
            watch_provider_status: None,
            watch_work_status: None,
            watch_long_wait: None,
            message: request.message,
        };
        self.pending_external_receive_acks.insert(
            message_id.clone(),
            crate::harness::PendingExternalReceiveAck {
                session_generation,
                recipient_id: recipient_id.clone(),
                recipient: request.recipient,
                expected_receive: received.clone(),
                canceled: false,
                started,
                reselect_attempted: false,
                rate_admitted_at,
                completion: crate::harness::PendingPeerReceiveCompletion::Remote {
                    client_id,
                    request_id: request.request_id,
                },
            },
        );
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::AgentMessageReceived(received),
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn handle_external_agent_message_request_without_auth_for_test(
        &mut self,
        request: tau_proto::ExternalAgentMessageRequest,
    ) -> tau_proto::ExternalAgentMessageResult {
        let request_id = request.request_id.clone();
        let result = self.receive_external_agent_message(request);
        let (recipient_id, started, error) = match result {
            Ok((recipient_id, started)) => (Some(recipient_id), started, None),
            Err(error) => (None, false, Some(error)),
        };
        tau_proto::ExternalAgentMessageResult {
            request_id,
            error,
            recipient_id,
            started,
        }
    }

    #[cfg(test)]
    fn receive_external_agent_message(
        &mut self,
        request: tau_proto::ExternalAgentMessageRequest,
    ) -> Result<(AgentId, bool), String> {
        self.validate_external_agent_message_target(&request)?;
        let (recipient_id, started, _rate_admitted_at) = match &request.recipient {
            tau_proto::ExternalAgentMessageRecipient::Exact(agent_id) => {
                let admitted_at = self.admit_peer_input(agent_id, request.message.len())?;
                (agent_id.clone(), false, admitted_at)
            }
            tau_proto::ExternalAgentMessageRecipient::BareEntrypoint => {
                self.resolve_peer_entrypoint_recipient(&request.message_id, request.message.len())?
            }
        };
        self.publish_event(
            Some(crate::harness::harness_connection_id()),
            Event::AgentMessageReceived(AgentMessageReceived {
                message_id: request.message_id,
                sender_id: request.sender_id,
                sender_session_id: Some(request.sender_session_id),
                recipient_id: recipient_id.clone(),
                kind: request.kind,
                watch_turn_state: None,
                watch_provider_status: None,
                watch_work_status: None,
                watch_long_wait: None,
                message: request.message,
            }),
        );
        self.record_peer_route(&recipient_id);
        self.uncommitted_peer_auto_starts.remove(&recipient_id);
        Ok((recipient_id, started))
    }

    fn validate_external_agent_message_target(
        &self,
        request: &tau_proto::ExternalAgentMessageRequest,
    ) -> Result<(), String> {
        if request.recipient_session_id != self.current_session_id {
            return Err(format!(
                "target harness is on active session `{}`, not `{}`",
                self.current_session_id, request.recipient_session_id
            ));
        }
        if request.message.trim().is_empty() {
            return Err("`message` must not be empty".to_owned());
        }
        let tau_proto::ExternalAgentMessageRecipient::Exact(recipient_id) = &request.recipient
        else {
            return self
                .inter_session_receivers
                .first()
                .map(|_| ())
                .ok_or_else(|| INTER_SESSION_UNAVAILABLE.to_owned());
        };
        match self.agent_message_recipient_status(recipient_id.as_str()) {
            AgentMessageRecipientStatus::Live => {}
            AgentMessageRecipientStatus::Stopped => {
                return Err(format!("stopped message recipient: `{}`", recipient_id));
            }
            AgentMessageRecipientStatus::Unknown => {
                return Err(format!("unknown message recipient: `{}`", recipient_id));
            }
        }
        Ok(())
    }

    fn validate_external_agent_message_syntax(
        &self,
        request: &tau_proto::ExternalAgentMessageRequest,
    ) -> Result<(), String> {
        if request.recipient_session_id != self.current_session_id {
            return Err("target session unavailable".to_owned());
        }
        if request.message.trim().is_empty() {
            return Err("`message` must not be empty".to_owned());
        }
        if request.message.len() > MAX_EXTERNAL_AGENT_MESSAGE_BYTES {
            return Err("peer message exceeds the 64 KiB limit".to_owned());
        }
        Ok(())
    }

    fn select_peer_entrypoint_recipient(&self) -> Result<AgentId, String> {
        if self.inter_session_receivers.is_empty() {
            return Err(INTER_SESSION_UNAVAILABLE.to_owned());
        }
        let role_available = |role: &str| {
            self.inter_session_receivers
                .iter()
                .any(|receiver| receiver.role == role)
                && self.available_roles.contains_key(role)
                && crate::model::model_for_role(
                    &self.provider_model_info,
                    &self.available_roles,
                    role,
                )
                .is_some()
        };
        let mut candidates = self
            .agents
            .values()
            .filter(|agent| !agent.terminating)
            .filter_map(|agent| {
                let id = agent.agent_id.as_ref()?;
                let role = agent.role.as_deref()?;
                role_available(role).then(|| {
                    (
                        usize::from(
                            agent.published_runtime_state == tau_proto::AgentRuntimeState::Running,
                        ),
                        self.peer_last_routed.get(id).copied().unwrap_or(0),
                        id.clone(),
                    )
                })
            })
            .chain(
                self.pending_start_agent_requests
                    .iter()
                    .filter(|pending| role_available(&pending.role))
                    .map(|pending| {
                        (
                            0,
                            self.peer_last_routed
                                .get(&pending.agent_id)
                                .copied()
                                .unwrap_or(0),
                            pending.agent_id.clone(),
                        )
                    }),
            )
            .collect::<Vec<_>>();
        candidates.sort();
        candidates
            .into_iter()
            .next()
            .map(|(_, _, id)| crate::parse_agent_id(&id))
            .ok_or_else(|| INTER_SESSION_UNAVAILABLE.to_owned())
    }

    /// Resolve a bare session message to an existing endpoint or start the
    /// first available authorized role.
    ///
    /// The event loop serializes this operation. A newly created endpoint is
    /// inserted before the next request is handled, which provides live
    /// single-flight coalescing without durable crash deduplication.
    pub(crate) fn resolve_peer_entrypoint_recipient(
        &mut self,
        message_id: &tau_proto::AgentMessageId,
        message_bytes: usize,
    ) -> Result<(AgentId, bool, Instant), String> {
        if let Ok(recipient_id) = self.select_peer_entrypoint_recipient() {
            let admitted_at = self.admit_peer_input(&recipient_id, message_bytes)?;
            return Ok((recipient_id, false, admitted_at));
        }
        let role = self
            .inter_session_receivers
            .iter()
            .filter(|receiver| receiver.auto_start)
            .find_map(|receiver| {
                self.available_roles
                    .contains_key(&receiver.role)
                    .then(|| {
                        crate::model::model_for_role(
                            &self.provider_model_info,
                            &self.available_roles,
                            &receiver.role,
                        )
                    })
                    .flatten()
                    .map(|_| receiver.role.clone())
            })
            .ok_or_else(|| INTER_SESSION_UNAVAILABLE.to_owned())?;
        // Resolve role, model, required skills, and the ordinary endpoint shape
        // before spending. `prepare_start_agent_request` mints identity but does
        // not create an agent or dispatch model work.
        let query = tau_proto::StartAgentRequest {
            query_id: format!("{PEER_AUTO_START_QUERY_PREFIX}{message_id}"),
            instruction: String::new(),
            role: Some(role),
            input_stats: tau_proto::ToolUseStats::default(),
            tool_call_id: None,
            task_name: None,
            parent_agent: None,
        };
        let pending = self
            .prepare_start_agent_request(crate::harness::harness_connection_id(), query)?
            .ok_or_else(|| INTER_SESSION_UNAVAILABLE.to_owned())?;
        let recipient_id = crate::parse_agent_id(&pending.agent_id);
        let admitted_at = self.admit_peer_input(&recipient_id, message_bytes)?;
        if let Err(error) = self.start_peer_agent_request(pending) {
            self.release_peer_input_rate(&recipient_id, admitted_at);
            if let Some(cid) = self.agent_routes.get(recipient_id.as_str()).cloned() {
                self.remove_agent(&cid);
            }
            return Err(format!("{INTER_SESSION_UNAVAILABLE}: {error}"));
        }
        self.uncommitted_peer_auto_starts
            .insert(recipient_id.clone());
        Ok((recipient_id, true, admitted_at))
    }

    /// Enforce endpoint queue, byte, and rolling-rate limits before acceptance.
    pub(crate) fn admit_peer_input(
        &mut self,
        recipient_id: &AgentId,
        message_bytes: usize,
    ) -> Result<Instant, String> {
        let loaded_wake_bytes = self
            .agent_routes
            .get(recipient_id.as_str())
            .and_then(|cid| self.agents.get(cid))
            .into_iter()
            .flat_map(|agent| &agent.pending_message_wakes)
            .filter_map(|wake| wake.source.peer_admission_bytes());
        let pending_start_wake_bytes = self
            .pending_start_agent_requests
            .iter()
            .find(|pending| pending.agent_id == recipient_id.as_str())
            .into_iter()
            .flat_map(|pending| &pending.pending_agent_message_wakes)
            .filter_map(|wake| wake.source.peer_admission_bytes());
        let parked_receive_bytes = self
            .pending_external_receive_acks
            .values()
            .filter(|pending| &pending.recipient_id == recipient_id && !pending.canceled)
            .map(|pending| pending.expected_receive.message.len());
        let (queued_count, queued_bytes) = loaded_wake_bytes
            .chain(pending_start_wake_bytes)
            .chain(parked_receive_bytes)
            .fold((0usize, 0usize), |(count, bytes), message_bytes| {
                (count.saturating_add(1), bytes.saturating_add(message_bytes))
            });
        if queued_count >= MAX_QUEUED_PEER_INPUTS_PER_AGENT
            || queued_bytes.saturating_add(message_bytes) > MAX_QUEUED_PEER_BYTES_PER_AGENT
        {
            return Err("peer input queue is full; retry later".to_owned());
        }
        let now = Instant::now();
        let accepted = self
            .peer_input_rate
            .entry(recipient_id.clone())
            .or_default();
        while accepted.front().is_some_and(|accepted_at| {
            now.saturating_duration_since(*accepted_at) >= Duration::from_secs(60)
        }) {
            accepted.pop_front();
        }
        if accepted.len() >= MAX_ACCEPTED_PEER_INPUTS_PER_MINUTE {
            return Err("peer input rate limit reached; retry later".to_owned());
        }
        accepted.push_back(now);
        Ok(now)
    }

    /// Release a rolling-rate slot for a receive rejected before commit.
    pub(crate) fn release_peer_input_rate(&mut self, recipient_id: &AgentId, admitted_at: Instant) {
        let Some(accepted) = self.peer_input_rate.get_mut(recipient_id) else {
            return;
        };
        if let Some(index) = accepted
            .iter()
            .position(|timestamp| *timestamp == admitted_at)
        {
            accepted.remove(index);
        }
        if accepted.is_empty() {
            self.peer_input_rate.remove(recipient_id);
        }
    }

    /// Revalidate one concrete endpoint against current receiver-role,
    /// provider/model, skill, and termination authority.
    pub(crate) fn peer_entrypoint_recipient_is_eligible(&self, recipient_id: &AgentId) -> bool {
        if self.inter_session_receivers.is_empty() {
            return false;
        }
        let role_available = |role: &str| {
            self.inter_session_receivers
                .iter()
                .any(|receiver| receiver.role == role)
                && self.available_roles.contains_key(role)
                && crate::model::model_for_role(
                    &self.provider_model_info,
                    &self.available_roles,
                    role,
                )
                .is_some()
        };
        self.agents.values().any(|agent| {
            !agent.terminating
                && agent.agent_id.as_deref() == Some(recipient_id.as_str())
                && agent.role.as_deref().is_some_and(role_available)
        }) || self.pending_start_agent_requests.iter().any(|pending| {
            pending.agent_id == recipient_id.as_str() && role_available(&pending.role)
        })
    }

    /// Record a successful concrete route for deterministic
    /// least-recently-routed selection.
    pub(crate) fn record_peer_route(&mut self, recipient_id: &AgentId) {
        self.peer_route_clock = self.peer_route_clock.saturating_add(1);
        self.peer_last_routed
            .insert(recipient_id.to_string(), self.peer_route_clock);
    }

    #[cfg(test)]
    pub(crate) fn handle_message_tool_call(
        &mut self,
        agent_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        self.ensure_harness_owned_tool_tracking(agent_id, call, &visible_tool_name);
        let result = parse_message_args(&call.arguments).and_then(|parsed| {
            self.publish_agent_message_from_agent(agent_id, parsed.recipient_id, parsed.message)
        });
        match result {
            Ok(()) => self.finish_harness_owned_tool_with_result(
                agent_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                "Message sent".to_owned(),
                None,
            ),
            Err(message) => self.finish_harness_owned_tool_with_error(
                agent_id,
                call_id,
                visible_tool_name,
                call.tool_type,
                message,
                Some(call.arguments.clone()),
            ),
        }
        Ok(())
    }

    /// Handle the harness-owned `wait` tool call inline.
    pub(crate) fn handle_wait_tool_call(
        &mut self,
        agent_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        self.handle_wait_tool_call_at(agent_id, call, visible_tool_name, Instant::now())
    }

    /// Handle the harness-owned `wait` tool call at one supplied monotonic
    /// time.
    ///
    /// Production supplies [`Instant::now`]; deterministic scheduler tests
    /// supply a fixed clock value.
    pub(crate) fn handle_wait_tool_call_at(
        &mut self,
        agent_id: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
        now: Instant,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        if let Some(call_ref) = call.call_ref {
            self.subagents
                .wait_tracker
                .retain_call_ref(call_id.clone(), call_ref);
        }
        self.ensure_harness_owned_tool_tracking(agent_id, call, &visible_tool_name);
        let parsed = parse_wait_args(&call.arguments);
        let wait_observation = call.call_ref.map(|_| tau_proto::ObservationId::random());
        let observed_mode = match &parsed {
            Ok(WaitTarget::Exact(target)) => self
                .subagents
                .wait_tracker
                .call_ref(target)
                .map_or(tau_proto::ToolWaitMode::ExactUnresolved, |target| {
                    tau_proto::ToolWaitMode::Exact { target }
                }),
            Ok(WaitTarget::AnyBackground) => tau_proto::ToolWaitMode::NextBackground,
            Ok(WaitTarget::AnyInput(timeout)) => tau_proto::ToolWaitMode::ActivatingInput {
                effective_timeout_minutes: u16::try_from(timeout.as_secs() / 60)
                    .unwrap_or(u16::MAX),
            },
            Err(_) => tau_proto::ToolWaitMode::InvalidArguments,
        };
        if let (Some(observation_id), Some(wait_call)) = (wait_observation, call.call_ref) {
            self.append_best_effort_observation(
                agent_id,
                observation_id,
                Event::AgentToolWaitObserved(tau_proto::AgentToolWaitObserved {
                    wait_call,
                    mode: observed_mode,
                }),
            );
        }
        let consumable_completion = match &parsed {
            Ok(WaitTarget::Exact(target)) => self
                .subagents
                .wait_tracker
                .completed_call_is_owned_by(target, agent_id)
                .then(|| target.clone()),
            Ok(WaitTarget::AnyBackground) => self
                .subagents
                .wait_tracker
                .oldest_completed_for_owner(agent_id),
            Ok(WaitTarget::AnyInput(_)) | Err(_) => None,
        };
        if self.has_pending_wait_preempting_prompt(agent_id, consumable_completion.as_ref()) {
            let activation =
                self.pending_wait_preempting_activation(agent_id, consumable_completion.as_ref());
            let wait = WaitRequest {
                call_id: call_id.clone(),
                tool_name: visible_tool_name.clone(),
                owner: agent_id.clone(),
                display_args: String::new(),
                call_ref: call.call_ref,
                wait_observation,
                registration: None,
            };
            let settlement_outcome = activation.map(|activation| match &parsed {
                Ok(WaitTarget::AnyInput(_)) => {
                    tau_proto::ToolWaitOutcome::InputAvailable { activation }
                }
                Ok(WaitTarget::Exact(target))
                    if !self
                        .subagents
                        .wait_tracker
                        .call_is_owned_by(target, agent_id) =>
                {
                    tau_proto::ToolWaitOutcome::Rejected {
                        reason: tau_proto::WaitRejectionReason::UnknownTarget,
                    }
                }
                Ok(WaitTarget::Exact(_)) | Ok(WaitTarget::AnyBackground) => {
                    tau_proto::ToolWaitOutcome::InterruptedByActivation { activation }
                }
                Err(_) => tau_proto::ToolWaitOutcome::Rejected {
                    reason: tau_proto::WaitRejectionReason::InvalidArguments,
                },
            });
            let mut reply = match parsed {
                Ok(WaitTarget::Exact(target)) => {
                    if !self
                        .subagents
                        .wait_tracker
                        .call_is_owned_by(&target, agent_id)
                    {
                        wait_error_reply(
                            call_id,
                            visible_tool_name,
                            format!("unknown tool call: `{target}`"),
                            None,
                        )
                    } else {
                        let source_tool_name = self.subagents.wait_tracker.call_tool_name(&target);
                        wait_interrupted_reply(
                            call_id,
                            visible_tool_name,
                            source_tool_name,
                            &target,
                        )
                    }
                }
                Ok(WaitTarget::AnyBackground) => {
                    wait_interrupted_any_reply(call_id, visible_tool_name)
                }
                Ok(WaitTarget::AnyInput(timeout)) => wait_input_available_reply(
                    call_id,
                    visible_tool_name,
                    wait_timeout_args(timeout),
                ),
                Err(message) => wait_error_reply(call_id, visible_tool_name, message, None),
            };
            if let Some(outcome) = settlement_outcome {
                reply = reply.with_settlement(&wait, outcome);
            }
            self.publish_wait_replies(vec![reply]);
            return Ok(());
        }
        let start = self.subagents.wait_tracker.handle_wait_invoke_at(
            agent_id,
            call_id,
            visible_tool_name,
            &call.arguments,
            now,
            wait_observation,
        );
        self.synchronize_work_waits_at(now);
        if let Some((observation_id, registration)) = start.registration.clone() {
            self.append_best_effort_observation(
                agent_id,
                observation_id,
                Event::AgentToolWaitRegistered(registration),
            );
        }
        if let Some(target) = start.suppress_call_id {
            self.suppress_background_completion_prompt(target);
        }
        self.publish_wait_replies(start.reply.into_iter().collect());
        Ok(())
    }

    fn has_pending_wait_preempting_prompt(
        &self,
        agent_id: &AgentId,
        consumable_completion: Option<&ToolCallId>,
    ) -> bool {
        if self.has_wait_preempting_message_wake(agent_id) {
            return true;
        }
        self.agents.get(agent_id).is_some_and(|agent| {
            // Accepted activation wakes active waits at queue time. This
            // level-triggered guard closes queue-before-register. A completion
            // prompt is ignored only when this exact/bare invocation can consume
            // that prompt's call, preserving completion arbitration without
            // hiding activating notices for other completed calls.
            agent.pending_replay_activation
                || agent.pending_prompts.iter().any(|prompt| {
                    prompt.creates_inference_activation()
                        && (!prompt.is_activating_background_completion()
                            || consumable_completion.is_none_or(|call_id| {
                                prompt.text != crate::harness::background_completion_prompt(call_id)
                            }))
                })
        })
    }

    fn pending_wait_preempting_activation(
        &self,
        agent_id: &AgentId,
        consumable_completion: Option<&ToolCallId>,
    ) -> Option<tau_proto::ObservationId> {
        let agent = self.agents.get(agent_id)?;
        agent
            .pending_message_wakes
            .iter()
            .find_map(|wake| wake.activation_observation)
            .or_else(|| {
                agent.pending_prompts.iter().find_map(|prompt| {
                    (prompt.creates_inference_activation()
                        && (!prompt.is_activating_background_completion()
                            || consumable_completion.is_none_or(|call_id| {
                                prompt.text != crate::harness::background_completion_prompt(call_id)
                            })))
                    .then_some(prompt.activation_observation)
                    .flatten()
                })
            })
    }

    pub(crate) fn ensure_harness_owned_tool_tracking(
        &mut self,
        cid: &AgentId,
        call: &AgentToolCall,
        visible_tool_name: &ToolName,
    ) {
        if self.tool_agents.contains_key(&call.id)
            || self.peer_internal_tool_agents.contains_key(&call.id)
        {
            return;
        }
        self.tool_agents.insert(call.id.clone(), cid.clone());
        self.pending_tools.insert(
            call.id.clone(),
            crate::harness::PendingTool {
                name: visible_tool_name.clone(),
                internal_name: call.name.clone(),
                tool_type: call.tool_type,
                allows_provider_image: false,
            },
        );
        self.bump_tools_started_for(cid);
    }

    pub(crate) fn finish_harness_owned_tool_with_result(
        &mut self,
        cid: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        result: String,
        details: Option<CborValue>,
    ) {
        self.finish_harness_owned_tool_with_cbor_result(
            cid,
            call_id,
            tool_name,
            tool_type,
            details.unwrap_or(CborValue::Text(result)),
            None,
        );
    }

    pub(crate) fn finish_harness_owned_tool_with_cbor_result(
        &mut self,
        cid: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        result: CborValue,
        display: Option<tau_proto::ToolUseState>,
    ) {
        let result = ToolResult {
            call_id: call_id.clone(),
            tool_name,
            tool_type,
            result,
            provider_content: Vec::new(),
            kind: ToolResultKind::Final,
            display,
            originator: tau_proto::PromptOriginator::User,
        };
        let transcript_owner = self.harness_owned_terminal_transcript_owner(cid, &call_id);
        let _settlement = self.publish_terminal_tool_result(transcript_owner, None, result);
    }

    pub(crate) fn finish_harness_owned_tool_with_error(
        &mut self,
        cid: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        message: String,
        details: Option<CborValue>,
    ) {
        self.finish_harness_owned_tool_with_display_error(
            cid, call_id, tool_name, tool_type, message, details, None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_harness_owned_tool_with_display_error(
        &mut self,
        cid: &AgentId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        message: String,
        details: Option<CborValue>,
        display: Option<tau_proto::ToolUseState>,
    ) {
        let error = ToolError {
            call_id: call_id.clone(),
            tool_name,
            tool_type,
            message,
            details,
            display,
            originator: tau_proto::PromptOriginator::User,
        };
        let transcript_owner = self.harness_owned_terminal_transcript_owner(cid, &call_id);
        self.publish_terminal_tool_error(transcript_owner, None, error);
        if transcript_owner.is_none() {
            self.finish_harness_owned_tool_tracking(&call_id);
        }
    }

    pub(crate) fn finish_prebuilt_internal_tool_result(&mut self, result: ToolResult) {
        let call_id = result.call_id.clone();
        let Some(owner_cid) = self
            .tool_agents
            .get(&call_id)
            .or_else(|| self.peer_internal_tool_agents.get(&call_id))
            .cloned()
        else {
            return;
        };
        if self.tool_turn.is_backgrounded(&call_id) {
            self.handle_background_tool_result(crate::harness::harness_connection_id(), result);
        } else {
            let transcript_owner =
                self.harness_owned_terminal_transcript_owner(&owner_cid, &call_id);
            let _settlement = self.publish_terminal_tool_result(transcript_owner, None, result);
        }
    }

    pub(crate) fn finish_prebuilt_internal_tool_error(&mut self, error: ToolError) {
        let call_id = error.call_id.clone();
        let Some(owner_cid) = self
            .tool_agents
            .get(&call_id)
            .or_else(|| self.peer_internal_tool_agents.get(&call_id))
            .cloned()
        else {
            return;
        };
        if self.tool_turn.is_backgrounded(&call_id) {
            self.handle_background_tool_error(Some(crate::harness::harness_connection_id()), error);
        } else {
            let transcript_owner =
                self.harness_owned_terminal_transcript_owner(&owner_cid, &call_id);
            self.publish_terminal_tool_error(transcript_owner, None, error);
            if transcript_owner.is_none() {
                self.finish_harness_owned_tool_tracking(&call_id);
            }
        }
    }

    fn publish_wait_replies(&mut self, replies: Vec<WaitReply>) {
        for reply in replies {
            if let Some(call_id) = reply.unsuppress_call_id.clone() {
                self.unsuppress_background_completion_prompt(call_id);
            }
            if let Some(call_id) = reply.suppress_call_id.clone() {
                self.suppress_background_completion_prompt(call_id);
            }
            let wait_call_id = reply.wait_call_id.clone();
            let Some(cid) = self
                .tool_agents
                .get(&wait_call_id)
                .or_else(|| self.peer_internal_tool_agents.get(&wait_call_id))
                .cloned()
            else {
                continue;
            };
            let transcript_owner =
                self.harness_owned_terminal_transcript_owner(&cid, &wait_call_id);
            let ownerless = transcript_owner.is_none();
            let cause = match &reply.kind {
                WaitReplyKind::Result { .. } => tau_proto::ToolTerminalCause::Completed,
                WaitReplyKind::Error { .. } => tau_proto::ToolTerminalCause::ToolError,
            };
            let wait_terminal = transcript_owner
                .and_then(|owner| self.observe_tool_terminal(owner, &wait_call_id, cause));
            if wait_terminal.is_some()
                && let Some(settlement) = reply.settlement
            {
                self.pending_wait_settlements
                    .insert(wait_call_id.clone(), settlement);
            }
            let settlement = match reply.kind {
                WaitReplyKind::Result { result, display } => self.publish_terminal_tool_result(
                    transcript_owner,
                    None,
                    ToolResult {
                        call_id: reply.wait_call_id,
                        tool_name: reply.wait_tool_name,
                        tool_type: ToolType::Function,
                        result,
                        provider_content: Vec::new(),
                        kind: ToolResultKind::Final,
                        display,
                        originator: tau_proto::PromptOriginator::User,
                    },
                ),
                WaitReplyKind::Error {
                    message,
                    details,
                    display,
                } => self.publish_terminal_tool_error(
                    transcript_owner,
                    None,
                    ToolError {
                        call_id: reply.wait_call_id,
                        tool_name: reply.wait_tool_name,
                        tool_type: ToolType::Function,
                        message,
                        details,
                        display,
                        originator: tau_proto::PromptOriginator::User,
                    },
                ),
            };
            if ownerless && matches!(settlement, TerminalSettlement::Caller) {
                self.finish_harness_owned_tool_tracking(&wait_call_id);
            }
        }
    }
}

/// State needed to complete an external message tool call after async delivery.
pub(crate) struct ExternalMessageToolCompletion {
    /// Conversation that owns the tool call.
    pub(crate) conversation_id: AgentId,
    /// Session generation active when the tool call was dispatched.
    pub(crate) session_generation: u64,
    /// Tool call id to complete.
    pub(crate) call_id: ToolCallId,
    /// Visible tool name for the result/error.
    pub(crate) tool_name: ToolName,
    /// Tool type declared on the call.
    pub(crate) tool_type: ToolType,
    /// Original arguments used as error details.
    pub(crate) details: CborValue,
}

fn random_external_message_capability(rng: &mut rand::rngs::StdRng) -> String {
    use rand::Rng as _;
    use rand::distributions::Alphanumeric;

    rng.sample_iter(Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}

fn authenticate_external_agent_message_sender(
    request: &tau_proto::ExternalAgentMessageRequest,
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    let deadline = Instant::now() + EXTERNAL_AGENT_MESSAGE_AUTH_TIMEOUT;
    let harness_path =
        bounded_runtime_lookup(request.sender_session_id.as_str(), deadline, cancelled)
            .map_err(|err| format!("failed to find sender harness: {err}"))?
            .ok_or_else(|| {
                format!(
                    "no running daemon for sender session `{}`",
                    request.sender_session_id
                )
            })?;
    let socket = crate::runtime_dir::socket_path(&harness_path);
    check_peer_io_active(deadline, cancelled)?;
    let mut peer = tau_socket::SocketPeer::connect_with_io_timeout(
        &socket,
        deadline.saturating_duration_since(Instant::now()),
    )
    .map_err(|err| format!("failed to connect to sender harness: {err}"))?;
    check_peer_io_active(deadline, cancelled)?;
    peer.set_write_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|err| format!("failed to set external auth deadline: {err}"))?;
    peer.send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: tau_proto::ExtensionName::parse(
            crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME,
        )
        .map_err(|error| format!("invalid external-message client name: {error}"))?,
        client_kind: tau_proto::ClientKind::External,
        capabilities: Default::default(),
    }))
    .map_err(|err| format!("failed to send external auth hello: {err}"))?;
    check_peer_io_active(deadline, cancelled)?;
    peer.set_write_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|err| format!("failed to set external auth deadline: {err}"))?;
    let auth_request_id = format!("auth-{}", request.request_id);
    peer.send(&tau_proto::HarnessInputMessage::ExternalAgentMessageAuth(
        tau_proto::ExternalAgentMessageAuthRequest {
            request_id: auth_request_id.clone(),
            message_id: request.message_id.clone(),
            capability: request.capability.clone(),
            sender_session_id: request.sender_session_id.clone(),
            sender_id: request.sender_id.clone(),
            recipient_session_id: request.recipient_session_id.clone(),
            recipient: request.recipient.clone(),
            kind: request.kind,
            message: request.message.clone(),
        },
    ))
    .map_err(|err| format!("failed to send external auth request: {err}"))?;
    loop {
        check_peer_io_active(deadline, cancelled)?;
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            return Err(format!(
                "timed out after {}s waiting for external message authentication",
                EXTERNAL_AGENT_MESSAGE_AUTH_TIMEOUT.as_secs()
            ));
        };
        match peer
            .recv_timeout(timeout.min(Duration::from_millis(100)))
            .map_err(|err| format!("failed to receive external auth result: {err}"))?
        {
            tau_socket::SocketReceive::Message {
                message: tau_proto::HarnessOutputMessage::ExternalAgentMessageAuthResult(result),
            } if result.request_id == auth_request_id => {
                if result.authorized {
                    return Ok(());
                }
                return Err(result
                    .error
                    .unwrap_or_else(|| "sender rejected external message capability".to_owned()));
            }
            tau_socket::SocketReceive::Message { .. } => continue,
            tau_socket::SocketReceive::Timeout => {
                continue;
            }
            tau_socket::SocketReceive::Closed => {
                return Err("sender harness closed before external auth result".to_owned());
            }
        }
    }
}

fn send_external_agent_message_request(
    request: tau_proto::ExternalAgentMessageRequest,
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<(AgentId, bool), String> {
    let deadline = Instant::now() + EXTERNAL_AGENT_MESSAGE_RESULT_TIMEOUT;
    let harness_path =
        bounded_runtime_lookup(request.recipient_session_id.as_str(), deadline, cancelled)
            .map_err(|err| err.to_string())?
            .ok_or_else(|| {
                format!(
                    "no running daemon for session `{}`",
                    request.recipient_session_id
                )
            })?;
    let socket = crate::runtime_dir::socket_path(&harness_path);
    check_peer_io_active(deadline, cancelled)?;
    let mut peer = tau_socket::SocketPeer::connect_with_io_timeout(
        &socket,
        deadline.saturating_duration_since(Instant::now()),
    )
    .map_err(|err| format!("failed to connect to target harness: {err}"))?;
    check_peer_io_active(deadline, cancelled)?;
    peer.set_write_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|err| format!("failed to set peer send deadline: {err}"))?;
    peer.send(&tau_proto::HarnessInputMessage::Hello(tau_proto::Hello {
        protocol_version: tau_proto::PROTOCOL_VERSION,
        client_name: tau_proto::ExtensionName::parse(
            crate::harness::EXTERNAL_AGENT_MESSAGE_CLIENT_NAME,
        )
        .map_err(|error| format!("invalid external-message client name: {error}"))?,
        client_kind: tau_proto::ClientKind::External,
        capabilities: Default::default(),
    }))
    .map_err(|err| format!("failed to send external message hello: {err}"))?;
    check_peer_io_active(deadline, cancelled)?;
    peer.set_write_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|err| format!("failed to set peer send deadline: {err}"))?;
    peer.send(&tau_proto::HarnessInputMessage::ExternalAgentMessage(
        request.clone(),
    ))
    .map_err(|err| format!("failed to send external message request: {err}"))?;
    loop {
        check_peer_io_active(deadline, cancelled)?;
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            return Err(format!(
                "timed out after {}s waiting for external message result",
                EXTERNAL_AGENT_MESSAGE_RESULT_TIMEOUT.as_secs()
            ));
        };
        match peer
            .recv_timeout(timeout.min(Duration::from_millis(100)))
            .map_err(|err| format!("failed to receive external message result: {err}"))?
        {
            tau_socket::SocketReceive::Message {
                message: tau_proto::HarnessOutputMessage::ExternalAgentMessageResult(result),
            } if result.request_id == request.request_id => {
                if let Some(error) = result.error {
                    return Err(error);
                }
                return result
                    .recipient_id
                    .map(|recipient_id| (recipient_id, result.started))
                    .ok_or_else(|| {
                        "target harness returned success without a resolved recipient".to_owned()
                    });
            }
            tau_socket::SocketReceive::Message { .. } => continue,
            tau_socket::SocketReceive::Timeout => {
                continue;
            }
            tau_socket::SocketReceive::Closed => {
                return Err("target harness closed before external message result".to_owned());
            }
        }
    }
}

fn bounded_runtime_lookup(
    session_id: &str,
    deadline: Instant,
    cancelled: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<Option<std::path::PathBuf>, crate::runtime_dir::FindHarnessForSessionError> {
    let Some(permit) = RuntimeLookupPermit::try_acquire() else {
        return Err(crate::runtime_dir::FindHarnessForSessionError::Incomplete {
            session_id: session_id.to_owned(),
        });
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let owned_session_id = session_id.to_owned();
    let worker_session_id = owned_session_id.clone();
    let worker_cancelled = Arc::clone(cancelled);
    std::thread::spawn(move || {
        let _permit = permit;
        let result = crate::runtime_dir::find_harness_for_session_until(
            &worker_session_id,
            deadline,
            &worker_cancelled,
        );
        // This call is intentionally best-effort; preserve the existing discarded
        // result. ast-grep-ignore: let-underscore-call
        let _ = tx.send(result);
    });
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(crate::runtime_dir::FindHarnessForSessionError::Incomplete {
                session_id: owned_session_id,
            });
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(crate::runtime_dir::FindHarnessForSessionError::Incomplete {
                session_id: owned_session_id,
            });
        };
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(crate::runtime_dir::FindHarnessForSessionError::Incomplete {
                    session_id: owned_session_id,
                });
            }
        }
    }
}

fn check_peer_io_active(
    deadline: Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        return Err("peer message operation canceled".to_owned());
    }
    if Instant::now() >= deadline {
        return Err("peer message operation timed out".to_owned());
    }
    Ok(())
}

#[cfg(test)]
struct TestBuiltinTools;

#[cfg(test)]
impl crate::InternalToolHandler for TestBuiltinTools {
    fn tool_specs(&self) -> Vec<tau_proto::ToolSpec> {
        vec![
            test_tool_spec("skill", None),
            wait_tool_spec(),
            message_tool_spec(),
        ]
    }

    fn handles(&self, internal_tool_name: &ToolName) -> bool {
        matches!(
            internal_tool_name.as_str(),
            "skill" | WAIT_TOOL_NAME | MESSAGE_TOOL_NAME
        )
    }

    fn handle_event(
        &self,
        host: &mut crate::InternalToolHost<'_>,
        event: &tau_proto::Event,
    ) -> Result<(), HarnessError> {
        let tau_proto::Event::ToolStarted(started) = event else {
            return Ok(());
        };
        let Some((conversation_id, call, visible_tool_name)) = host.internal_started_call(started)
        else {
            return Ok(());
        };
        match call.name.as_str() {
            "skill" => Ok(()),
            WAIT_TOOL_NAME => {
                host.handle_wait_tool_call(&conversation_id, &call, visible_tool_name)
            }
            MESSAGE_TOOL_NAME => {
                host.handle_message_tool_call(&conversation_id, &call, visible_tool_name)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
fn message_tool_spec() -> tau_proto::ToolSpec {
    let mut spec = test_tool_spec(MESSAGE_TOOL_NAME, Some(tau_proto::BackgroundSupport::Never));
    spec.parameters = Some(serde_json::json!({
        "type":"object",
        "required":["recipient_id","message"]
    }));
    spec
}

#[cfg(test)]
fn wait_tool_spec() -> tau_proto::ToolSpec {
    test_tool_spec(WAIT_TOOL_NAME, Some(tau_proto::BackgroundSupport::Never))
}

#[cfg(test)]
fn test_tool_spec(
    name: &str,
    background_support: Option<tau_proto::BackgroundSupport>,
) -> tau_proto::ToolSpec {
    tau_proto::ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: Some(name.to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({"type":"object"})),
        format: None,
        tags: Vec::new(),
        enabled_by_default: true,
        background_support,
        examples: Vec::new(),
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
struct MessageArgs {
    recipient_id: String,
    message: String,
}

#[cfg(test)]
fn parse_message_args(arguments: &CborValue) -> Result<MessageArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut recipient_id = None;
    let mut message = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "recipient_id" => match v {
                CborValue::Text(text) => recipient_id = Some(text.clone()),
                _ => return Err("`recipient_id` must be a string".to_owned()),
            },
            "message" => match v {
                CborValue::Text(text) => message = Some(text.clone()),
                _ => return Err("`message` must be a string".to_owned()),
            },
            _ => {}
        }
    }
    let recipient_id = recipient_id.ok_or_else(|| "`recipient_id` is required".to_owned())?;
    if recipient_id.trim().is_empty() {
        return Err("`recipient_id` must not be empty".to_owned());
    }
    let message = message.ok_or_else(|| "`message` is required".to_owned())?;
    if message.trim().is_empty() {
        return Err("`message` must not be empty".to_owned());
    }
    Ok(MessageArgs {
        recipient_id,
        message,
    })
}

#[cfg(test)]
mod tests;

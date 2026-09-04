//! Pure scheduling state for one agent tool turn.
//!
//! The harness owns side effects (publishing, routing, and follow-up prompts).
//! This module only decides which queued tool invocation can dispatch next and
//! tracks calls that have been selected but not completed yet. Background
//! deadlines are measured from the dispatch instant recorded here, not from the
//! start of the agent turn, so queued calls do not spend their foreground
//! budget before they have actually started.

use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(test)]
use std::sync::Mutex;
use std::time as path_std_time;
use std::time::Instant;

use tau_proto::{AgentId, BackgroundSupport, ConnectionId, ToolCallId, ToolName, ToolType};

use crate::harness::AgentToolCall;

/// Recognized static turn-activity categories frozen for one tool invocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ToolTurnCategories {
    /// Recognized category bits; zero is reserved for an empty aggregate.
    bits: u8,
}

impl ToolTurnCategories {
    /// Derive recognized categories from a registered tool's neutral tags.
    pub(crate) fn from_tags(tags: &[tau_proto::ToolTag]) -> Self {
        let mut bits = 0;
        for tag in tags {
            bits |= match tag.as_ref() {
                tau_proto::TURN_MANIPULATOR_TOOL_TAG => 1,
                tau_proto::TURN_DATA_FETCH_TOOL_TAG => 2,
                tau_proto::TURN_WAIT_TOOL_TAG => 4,
                _ => 0,
            };
        }
        Self {
            bits: if bits == 0 { 1 } else { bits },
        }
    }

    /// Combine another active call's normalized categories.
    fn combine(&mut self, other: Self) {
        self.bits |= other.bits;
    }

    /// Convert an empty caller value into the conservative call fallback.
    fn normalized_call(self) -> Self {
        if self.bits == 0 {
            Self { bits: 1 }
        } else {
            self
        }
    }

    /// Whether any active call is manipulating.
    pub(crate) fn manipulator(self) -> bool {
        self.bits & 1 != 0
    }

    /// Whether any active call is fetching data.
    pub(crate) fn data_fetch(self) -> bool {
        self.bits & 2 != 0
    }

    /// Whether any active call is waiting.
    pub(crate) fn wait(self) -> bool {
        self.bits & 4 != 0
    }
}

/// A tool call emitted by an agent response but not yet completed.
#[derive(Debug)]
pub(crate) struct PendingToolInvocation {
    /// Agent that owns the tool call.
    pub(crate) conversation_id: AgentId,
    /// Tool call payload to route when selected.
    pub(crate) invocation: AgentToolCall,
    /// Foreground/background support resolved at enqueue time.
    pub(crate) background_support: BackgroundSupport,
    /// Source to apply to report-derived dispatch successors.
    pub(crate) source: Option<ConnectionId>,
    /// Recognized static activity categories frozen at enqueue time.
    pub(crate) turn_categories: ToolTurnCategories,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PendingToolOwnershipWork {
    /// Deep clones of the complete queued invocation.
    pub(crate) pending_clones: usize,
    /// Borrowed scheduler candidates inspected before removal.
    pub(crate) candidate_visits: usize,
    /// Queue entries removed into dispatch ownership.
    pub(crate) queue_pops: usize,
    /// Address of the largest queued text allocation at admission.
    pub(crate) admission_text_ptr: usize,
    /// Address of that allocation after removal from the queue.
    pub(crate) popped_text_ptr: usize,
    /// Address of that allocation on entry to execution.
    pub(crate) execution_text_ptr: usize,
}

#[cfg(test)]
static PENDING_TOOL_OWNERSHIP_PROBES: Mutex<Vec<(String, PendingToolOwnershipWork)>> =
    Mutex::new(Vec::new());

#[cfg(test)]
impl Clone for PendingToolInvocation {
    fn clone(&self) -> Self {
        record_pending_tool_ownership(&self.invocation.id, |work| {
            work.pending_clones += 1;
        });
        Self {
            conversation_id: self.conversation_id.clone(),
            invocation: self.invocation.clone(),
            background_support: self.background_support,
            source: self.source.clone(),
            turn_categories: self.turn_categories,
        }
    }
}

#[cfg(test)]
pub(crate) fn start_pending_tool_ownership_probe(call_id: &str) {
    let mut probes = PENDING_TOOL_OWNERSHIP_PROBES
        .lock()
        .expect("pending tool ownership probe poisoned");
    assert!(
        probes.iter().all(|(existing, _)| existing != call_id),
        "pending tool ownership probe already active for {call_id}"
    );
    probes.push((call_id.to_owned(), PendingToolOwnershipWork::default()));
}

#[cfg(test)]
pub(crate) fn finish_pending_tool_ownership_probe(call_id: &str) -> PendingToolOwnershipWork {
    let mut probes = PENDING_TOOL_OWNERSHIP_PROBES
        .lock()
        .expect("pending tool ownership probe poisoned");
    let position = probes
        .iter()
        .position(|(existing, _)| existing == call_id)
        .expect("pending tool ownership probe was started");
    probes.swap_remove(position).1
}

#[cfg(test)]
pub(crate) fn record_pending_tool_execution(call: &AgentToolCall) {
    record_pending_tool_ownership(&call.id, |work| {
        work.execution_text_ptr = largest_text_ptr(&call.arguments);
    });
}

#[cfg(test)]
fn record_pending_tool_ownership(
    call_id: &ToolCallId,
    update: impl FnOnce(&mut PendingToolOwnershipWork),
) {
    let mut probes = PENDING_TOOL_OWNERSHIP_PROBES
        .lock()
        .expect("pending tool ownership probe poisoned");
    if let Some((_, work)) = probes
        .iter_mut()
        .find(|(expected, _)| expected == call_id.as_str())
    {
        update(work);
    }
}

#[cfg(test)]
fn largest_text_ptr(value: &tau_proto::CborValue) -> usize {
    fn largest_text(value: &tau_proto::CborValue) -> Option<&str> {
        match value {
            tau_proto::CborValue::Text(text) => Some(text),
            tau_proto::CborValue::Array(values) => values
                .iter()
                .filter_map(largest_text)
                .max_by_key(|text| text.len()),
            tau_proto::CborValue::Map(entries) => entries
                .iter()
                .flat_map(|(key, value)| [largest_text(key), largest_text(value)])
                .flatten()
                .max_by_key(|text| text.len()),
            _ => None,
        }
    }
    largest_text(value).map_or(0, |text| text.as_ptr() as usize)
}

/// Pure queue and in-flight state for tool dispatch during agent turns.
#[derive(Debug, Default)]
pub(crate) struct ToolTurnMachine {
    /// Tool invocations waiting for dispatch.
    pending_tool_invocations: VecDeque<PendingToolInvocation>,
    /// Tool calls selected for dispatch and still actually running.
    in_flight_tool_invocations: HashMap<ToolCallId, InFlightToolInvocation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ForegroundAction {
    /// Nothing should be published to close the foreground yet.
    None,
    /// Publish a synthetic terminal tool result for this call.
    Background { call_id: ToolCallId },
}

#[derive(Clone, Debug)]
struct InFlightToolInvocation {
    conversation_id: AgentId,
    /// Recognized static categories frozen before dispatch.
    turn_categories: ToolTurnCategories,
    foreground_pending: bool,
    /// The sole placeholder is reserved but has not committed yet.
    backgrounding: bool,
    backgrounded: bool,
    foreground_deadline: Option<Instant>,
}

impl ToolTurnMachine {
    /// Enqueue one tool invocation at the back of the turn queue.
    #[cfg(test)]
    pub(crate) fn push(
        &mut self,
        conversation_id: AgentId,
        invocation: AgentToolCall,
        background_support: BackgroundSupport,
    ) {
        self.push_from(
            conversation_id,
            invocation,
            background_support,
            None,
            ToolTurnCategories::default(),
        );
    }

    /// Enqueue one tool invocation and retain source for derived facts.
    pub(crate) fn push_from(
        &mut self,
        conversation_id: AgentId,
        invocation: AgentToolCall,
        background_support: BackgroundSupport,
        source: Option<ConnectionId>,
        turn_categories: ToolTurnCategories,
    ) {
        #[cfg(test)]
        record_pending_tool_ownership(&invocation.id, |work| {
            work.admission_text_ptr = largest_text_ptr(&invocation.arguments);
        });
        self.pending_tool_invocations
            .push_back(PendingToolInvocation {
                conversation_id,
                invocation,
                background_support,
                source,
                turn_categories: turn_categories.normalized_call(),
            });
    }

    /// Returns the next invocation the scheduler would dispatch, without
    /// removing it or marking it in flight.
    pub(crate) fn next_dispatchable(&self) -> Option<&PendingToolInvocation> {
        let idx = self.next_dispatchable_index()?;
        let pending = self.pending_tool_invocations.get(idx)?;
        #[cfg(test)]
        record_pending_tool_ownership(&pending.invocation.id, |work| {
            work.candidate_visits += 1;
        });
        Some(pending)
    }

    /// Select the next dispatchable invocation and mark it in flight.
    pub(crate) fn pop_dispatchable(
        &mut self,
        now: Instant,
    ) -> Option<(PendingToolInvocation, ForegroundAction)> {
        let idx = self.next_dispatchable_index()?;
        let pending = self
            .pending_tool_invocations
            .remove(idx)
            .expect("index just located");
        #[cfg(test)]
        record_pending_tool_ownership(&pending.invocation.id, |work| {
            work.queue_pops += 1;
            work.popped_text_ptr = largest_text_ptr(&pending.invocation.arguments);
        });
        let action = self.record_in_flight(&pending, now);
        Some((pending, action))
    }

    /// Mark an invocation as in flight without queueing it first.
    pub(crate) fn record_unqueued_in_flight(
        &mut self,
        conversation_id: AgentId,
        call_id: ToolCallId,
        turn_categories: ToolTurnCategories,
    ) {
        self.in_flight_tool_invocations.insert(
            call_id,
            InFlightToolInvocation {
                conversation_id,
                foreground_pending: true,
                backgrounding: false,
                backgrounded: false,
                foreground_deadline: None,
                turn_categories: turn_categories.normalized_call(),
            },
        );
    }

    /// Remove a call from the in-flight set after its real result arrives.
    pub(crate) fn mark_complete(&mut self, call_id: &ToolCallId) -> bool {
        self.in_flight_tool_invocations.remove(call_id).is_some()
    }

    /// Roll back an in-flight mark after synchronous dispatch failure.
    pub(crate) fn rollback_dispatch(&mut self, call_id: &ToolCallId) -> bool {
        self.mark_complete(call_id)
    }

    /// Reserve the sole provider-facing background placeholder for a call.
    ///
    /// The call remains foreground-pending until that placeholder commits.
    pub(crate) fn begin_backgrounding(&mut self, call_id: &ToolCallId) -> bool {
        let Some(in_flight) = self.in_flight_tool_invocations.get_mut(call_id) else {
            return false;
        };
        if !in_flight.foreground_pending || in_flight.backgrounding {
            return false;
        }
        in_flight.backgrounding = true;
        in_flight.foreground_deadline = None;
        true
    }

    /// Mark one running call as completed in the foreground after its durable
    /// background placeholder commits. The real call remains actual-running.
    pub(crate) fn mark_backgrounded(&mut self, call_id: &ToolCallId) -> bool {
        let Some(in_flight) = self.in_flight_tool_invocations.get_mut(call_id) else {
            return false;
        };
        if !in_flight.foreground_pending || !in_flight.backgrounding {
            return false;
        }
        in_flight.foreground_pending = false;
        in_flight.backgrounding = false;
        in_flight.backgrounded = true;
        in_flight.foreground_deadline = None;
        true
    }

    /// Restore one durably placeholdered call as still running in background.
    pub(crate) fn restore_backgrounded(&mut self, conversation_id: AgentId, call_id: ToolCallId) {
        self.in_flight_tool_invocations
            .entry(call_id)
            .or_insert(InFlightToolInvocation {
                conversation_id,
                turn_categories: ToolTurnCategories::default().normalized_call(),
                foreground_pending: false,
                backgrounding: false,
                backgrounded: true,
                foreground_deadline: None,
            });
    }

    /// True when this call has already been completed in the foreground but is
    /// still actually running.
    pub(crate) fn is_backgrounded(&self, call_id: &ToolCallId) -> bool {
        self.in_flight_tool_invocations
            .get(call_id)
            .is_some_and(|in_flight| in_flight.backgrounded)
    }

    /// Backgrounded calls still actually running for `conversation_id`.
    pub(crate) fn backgrounded_calls_for(&self, conversation_id: &AgentId) -> Vec<ToolCallId> {
        self.in_flight_tool_invocations
            .iter()
            .filter_map(|(call_id, in_flight)| {
                (&in_flight.conversation_id == conversation_id && in_flight.backgrounded)
                    .then_some(call_id.clone())
            })
            .collect()
    }

    /// Return calls whose foreground deadline has expired.
    pub(crate) fn background_due(&self, now: Instant) -> Vec<ToolCallId> {
        self.in_flight_tool_invocations
            .iter()
            .filter_map(|(call_id, in_flight)| {
                (in_flight.foreground_pending
                    && !in_flight.backgrounding
                    && in_flight
                        .foreground_deadline
                        .is_some_and(|deadline| deadline <= now))
                .then_some(call_id.clone())
            })
            .collect()
    }

    /// Earliest foreground background deadline that still needs a wakeup.
    pub(crate) fn next_background_deadline(&self) -> Option<Instant> {
        self.in_flight_tool_invocations
            .values()
            .filter(|in_flight| in_flight.foreground_pending && !in_flight.backgrounding)
            .filter_map(|in_flight| in_flight.foreground_deadline)
            .min()
    }

    /// Remove all queued invocations for `conversation_id` whose call id is in
    /// `remaining`.
    pub(crate) fn cancel_queued_for(
        &mut self,
        conversation_id: &AgentId,
        remaining: &HashSet<ToolCallId>,
    ) -> Vec<(ToolCallId, ToolName, ToolType)> {
        let mut queued = Vec::new();
        self.pending_tool_invocations.retain(|pending| {
            let should_cancel = &pending.conversation_id == conversation_id
                && remaining.contains(&pending.invocation.id);
            if should_cancel {
                queued.push((
                    pending.invocation.id.clone(),
                    pending.invocation.name.clone(),
                    pending.invocation.tool_type,
                ));
            }
            !should_cancel
        });
        queued
    }

    /// Remove all queued and in-flight scheduler state.
    /// True when no queued or in-flight tool calls remain.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pending_tool_invocations.is_empty() && self.in_flight_tool_invocations.is_empty()
    }

    /// Number of queued invocations.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending_tool_invocations.len()
    }

    /// Number of in-flight invocations.
    #[cfg(test)]
    pub(crate) fn in_flight_len(&self) -> usize {
        self.in_flight_tool_invocations.len()
    }

    /// Whether a call is tracked as in-flight.
    #[cfg(test)]
    pub(crate) fn is_in_flight(&self, call_id: &ToolCallId) -> bool {
        self.in_flight_tool_invocations.contains_key(call_id)
    }

    /// Whether `conversation_id` has queued work.
    #[cfg(test)]
    pub(crate) fn any_pending_for(&self, conversation_id: &AgentId) -> bool {
        self.pending_tool_invocations
            .iter()
            .any(|pending| &pending.conversation_id == conversation_id)
    }

    /// Whether `conversation_id` has foreground in-flight work.
    pub(crate) fn any_in_flight_for(&self, conversation_id: &AgentId) -> bool {
        self.in_flight_tool_invocations.values().any(|in_flight| {
            &in_flight.conversation_id == conversation_id && in_flight.foreground_pending
        })
    }

    /// Aggregate recognized categories for all real active calls owned by an
    /// agent.
    pub(crate) fn active_categories_for(&self, conversation_id: &AgentId) -> ToolTurnCategories {
        self.in_flight_tool_invocations
            .values()
            .filter(|invocation| &invocation.conversation_id == conversation_id)
            .fold(
                ToolTurnCategories::default(),
                |mut aggregate, invocation| {
                    aggregate.combine(invocation.turn_categories);
                    aggregate
                },
            )
    }

    fn record_in_flight(
        &mut self,
        pending: &PendingToolInvocation,
        now: Instant,
    ) -> ForegroundAction {
        let (foreground_pending, backgrounded, foreground_deadline, action) =
            match pending.background_support {
                BackgroundSupport::Instant => (
                    true,
                    false,
                    None,
                    ForegroundAction::Background {
                        call_id: pending.invocation.id.clone(),
                    },
                ),
                BackgroundSupport::MinForegroundSeconds(seconds) => (
                    true,
                    false,
                    Some(now + path_std_time::Duration::from_secs(seconds)),
                    ForegroundAction::None,
                ),
                BackgroundSupport::Never => (true, false, None, ForegroundAction::None),
            };
        self.in_flight_tool_invocations.insert(
            pending.invocation.id.clone(),
            InFlightToolInvocation {
                conversation_id: pending.conversation_id.clone(),
                foreground_pending,
                backgrounding: false,
                backgrounded,
                foreground_deadline,
                turn_categories: pending.turn_categories.normalized_call(),
            },
        );
        action
    }

    fn next_dispatchable_index(&self) -> Option<usize> {
        (!self.pending_tool_invocations.is_empty()).then_some(0)
    }
}

#[cfg(test)]
mod tests;

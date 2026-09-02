//! Runtime-only immutable notification delivery deadlines.

use std::time::Instant;

use tau_config::settings::NotificationDeliveryPolicy;

#[cfg(test)]
mod tests;

/// Runtime state selecting one deadline from an admitted delivery schedule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryDeadlineKind {
    /// Target is dispatchable and idle.
    Idle,
    /// Target waits for any input or self-reports `Waiting`.
    WaitAny,
    /// Target waits for one or more exact tool calls.
    WaitTool,
    /// Target is busy with work that notification delivery cannot preempt.
    Unavailable,
}

/// Three immutable monotonic deadlines and sticky trigger readiness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeliverySchedule {
    /// Deadline selected while the target is idle.
    idle_deadline: Instant,
    /// Deadline selected for input, bare, or self-reported waiting.
    wait_any_deadline: Instant,
    /// Deadline selected for exact tool waiting.
    wait_tool_deadline: Instant,
    /// Whether any applicable deadline has already elapsed.
    ready: bool,
    /// Whether readiness side effects have been applied on the owning branch.
    activation_applied: bool,
}

impl DeliverySchedule {
    /// Anchor all deadlines to one post-admission monotonic cut.
    pub(crate) fn new(
        admitted_at: Instant,
        policy: NotificationDeliveryPolicy,
    ) -> Result<Self, String> {
        Ok(Self {
            idle_deadline: admitted_at.checked_add(policy.idle()).ok_or_else(|| {
                "notification idle delay exceeds the monotonic clock range".to_owned()
            })?,
            wait_any_deadline: admitted_at.checked_add(policy.wait_any()).ok_or_else(|| {
                "notification wait-any delay exceeds the monotonic clock range".to_owned()
            })?,
            wait_tool_deadline: admitted_at.checked_add(policy.wait_tool()).ok_or_else(|| {
                "notification wait-tool delay exceeds the monotonic clock range".to_owned()
            })?,
            ready: false,
            activation_applied: false,
        })
    }

    /// Construct a cold-replay obligation that is immediately trigger-ready.
    pub(crate) fn replay_ready(now: Instant) -> Self {
        Self {
            idle_deadline: now,
            wait_any_deadline: now,
            wait_tool_deadline: now,
            ready: true,
            activation_applied: false,
        }
    }

    /// Return whether the selected deadline is already due without mutating
    /// state.
    pub(crate) fn is_ready_at(&self, kind: DeliveryDeadlineKind, now: Instant) -> bool {
        self.ready || self.deadline(kind).is_some_and(|deadline| deadline <= now)
    }

    /// Return the currently applicable immutable deadline.
    pub(crate) fn deadline(&self, kind: DeliveryDeadlineKind) -> Option<Instant> {
        if self.ready {
            return None;
        }
        match kind {
            DeliveryDeadlineKind::Idle => Some(self.idle_deadline),
            DeliveryDeadlineKind::WaitAny => Some(self.wait_any_deadline),
            DeliveryDeadlineKind::WaitTool => Some(self.wait_tool_deadline),
            DeliveryDeadlineKind::Unavailable => None,
        }
    }

    /// Make the schedule permanently ready when its selected deadline is due.
    pub(crate) fn mark_ready_at(&mut self, kind: DeliveryDeadlineKind, now: Instant) -> bool {
        if self.ready || self.deadline(kind).is_none_or(|deadline| now < deadline) {
            return false;
        }
        self.ready = true;
        true
    }

    /// Claim readiness side effects exactly once after branch applicability.
    pub(crate) fn take_ready_activation(&mut self) -> bool {
        if !self.ready || self.activation_applied {
            return false;
        }
        self.activation_applied = true;
        true
    }
}

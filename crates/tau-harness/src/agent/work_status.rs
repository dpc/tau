//! Runtime-only semantic work-status state.

use std::time::{Duration, Instant};

use tau_proto::AgentWorkStatusPhase;

/// First whole-minute threshold for long-wait watcher notifications.
const FIRST_WAIT_THRESHOLD_MINUTES: u32 = 15;
/// Maximum number of committed successful finals challenged while one
/// unresolved status phase remains current.
const MAX_FINAL_CHALLENGES: u8 = 2;

/// Compact ordered range of crossed long-wait thresholds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CrossedWaitThresholds {
    /// Next threshold not yet materialized.
    next_minutes: u32,
    /// Number of thresholds remaining in the range.
    remaining: u32,
}

impl CrossedWaitThresholds {
    /// Pop the next threshold in approved sequence order.
    pub(crate) fn pop_next(&mut self) -> Option<u32> {
        if self.remaining == 0 {
            return None;
        }
        let threshold = self.next_minutes;
        self.remaining -= 1;
        if 0 < self.remaining {
            self.next_minutes =
                next_wait_threshold(threshold).expect("remaining threshold has successor");
        }
        Some(threshold)
    }

    /// Return whether every threshold has been materialized.
    pub(crate) fn is_empty(&self) -> bool {
        self.remaining == 0
    }
}

/// Validated model-reportable work status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkStatusReport {
    /// Closed model-reportable phase.
    phase: AgentWorkStatusPhase,
    /// Canonical bounded title.
    title: String,
}

impl WorkStatusReport {
    /// Validate and canonicalize one model-authored report.
    pub fn new(phase: AgentWorkStatusPhase, title: String) -> Result<Self, String> {
        if !matches!(
            phase,
            AgentWorkStatusPhase::Working
                | AgentWorkStatusPhase::Done
                | AgentWorkStatusPhase::Blocked
        ) {
            return Err("status state must be working, done, or blocked".to_owned());
        }
        let canonical = title.trim();
        if canonical.is_empty() {
            return Err("status task_name must not be empty".to_owned());
        }
        if 160 < canonical.len() {
            return Err("status task_name must be at most 160 UTF-8 bytes".to_owned());
        }
        if canonical
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
        {
            return Err("status task_name must be one line without control characters".to_owned());
        }
        Ok(Self {
            phase,
            title: canonical.to_owned(),
        })
    }

    /// Return the validated model-reportable phase.
    pub fn phase(&self) -> AgentWorkStatusPhase {
        self.phase
    }

    /// Return the canonical bounded title.
    pub fn title(&self) -> &str {
        &self.title
    }
}

/// Semantic unresolved status captured for one final challenge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FinalStatusChallenge {
    /// No status report has been accepted.
    Unreported,
    /// Working remains current with its canonical title.
    Working {
        /// Canonical title from the accepted Working report.
        title: String,
    },
}

impl FinalStatusChallenge {
    /// Return the closed phase represented by this challenge.
    fn phase(&self) -> AgentWorkStatusPhase {
        match self {
            Self::Unreported => AgentWorkStatusPhase::Unreported,
            Self::Working { .. } => AgentWorkStatusPhase::Working,
        }
    }
}

/// Named inputs to final-status gate evaluation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FinalStatusInput {
    /// Whether the provider response is a successful terminal.
    pub(crate) successful: bool,
    /// Whether the immutable dispatched prompt surface exposed `status`.
    pub(crate) status_was_available: bool,
}

/// Gate decision for one no-tool final with unresolved status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FinalStatusDecision {
    /// Retain the outer turn and continue after commit.
    Challenge(FinalStatusChallenge),
    /// Terminalize after commit, invalidating Working when current.
    Accept,
}

/// Runtime-only self-reported work state for one loaded agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkStatus {
    /// Current closed work phase.
    phase: AgentWorkStatusPhase,
    /// Canonical title, absent until the first accepted report.
    title: Option<String>,
    /// Runtime-local generation incremented by a new working epoch.
    epoch: u64,
    /// Whether the current foreground tool round began substantive work before
    /// an accepted Working report.
    working_reminder_pending: bool,
    /// Number of committed successful finals challenged while the current
    /// Unreported or Working phase remains unresolved.
    final_challenges_sent: u8,
    /// Wait duration completed during the current Working epoch.
    completed_wait: Duration,
    /// Monotonic start of the current union-of-waits interval.
    wait_started_at: Option<Instant>,
    /// Next whole-minute wait threshold not yet emitted.
    next_wait_threshold_minutes: Option<u32>,
    /// Immediate scheduler deadline retained while crossed thresholds await
    /// bounded catch-up.
    wait_catchup_deadline: Option<Instant>,
}

impl Default for WorkStatus {
    fn default() -> Self {
        Self {
            phase: AgentWorkStatusPhase::Unreported,
            title: None,
            epoch: 0,
            working_reminder_pending: false,
            final_challenges_sent: 0,
            completed_wait: Duration::ZERO,
            wait_started_at: None,
            next_wait_threshold_minutes: Some(FIRST_WAIT_THRESHOLD_MINUTES),
            wait_catchup_deadline: None,
        }
    }
}

impl WorkStatus {
    /// Return the current closed phase.
    pub(crate) fn phase(&self) -> AgentWorkStatusPhase {
        self.phase
    }

    /// Return the current canonical title, when reported.
    pub(crate) fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Return the runtime-local Working epoch.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Apply a validated model report at `now` and return whether its snapshot
    /// changed.
    pub(crate) fn report_at(
        &mut self,
        report: WorkStatusReport,
        now: Instant,
        wait_installed: bool,
    ) -> bool {
        let WorkStatusReport { phase, title } = report;
        if self.phase == phase && self.title.as_deref() == Some(title.as_str()) {
            if phase == AgentWorkStatusPhase::Working {
                self.working_reminder_pending = false;
                self.synchronize_wait_at(wait_installed, now);
            }
            return false;
        }
        if phase == AgentWorkStatusPhase::Working && self.phase != AgentWorkStatusPhase::Working {
            self.epoch = self.epoch.saturating_add(1);
            self.final_challenges_sent = 0;
            self.completed_wait = Duration::ZERO;
            self.wait_started_at = wait_installed.then_some(now);
            self.next_wait_threshold_minutes = Some(FIRST_WAIT_THRESHOLD_MINUTES);
            self.wait_catchup_deadline = None;
        } else if phase != AgentWorkStatusPhase::Working {
            self.wait_started_at = None;
            self.wait_catchup_deadline = None;
        }
        self.phase = phase;
        self.title = Some(title);
        if phase == AgentWorkStatusPhase::Working {
            self.working_reminder_pending = false;
        }
        true
    }

    /// Synchronize the union-of-installed-waits interval at one monotonic time.
    pub(crate) fn synchronize_wait_at(&mut self, wait_installed: bool, now: Instant) {
        if self.phase != AgentWorkStatusPhase::Working {
            self.wait_started_at = None;
            return;
        }
        match (self.wait_started_at, wait_installed) {
            (None, true) => self.wait_started_at = Some(now),
            (Some(started_at), false) => {
                self.completed_wait = self
                    .completed_wait
                    .saturating_add(now.saturating_duration_since(started_at));
                self.wait_started_at = None;
                if self.next_wait_threshold_is_crossed_at(now) {
                    self.wait_catchup_deadline = Some(now);
                }
            }
            (None, false) | (Some(_), true) => {}
        }
    }

    /// Retire an installed-wait interval at teardown without scheduling
    /// cleanup-time threshold delivery.
    pub(crate) fn retire_wait_at(&mut self, now: Instant) {
        self.synchronize_wait_at(false, now);
        self.wait_catchup_deadline = None;
    }

    /// Return the next active-wait or bounded-catch-up threshold deadline.
    pub(crate) fn next_wait_deadline(&self) -> Option<Instant> {
        if let Some(deadline) = self.wait_catchup_deadline {
            return Some(deadline);
        }
        let started_at = self.wait_started_at?;
        let threshold = wait_threshold_duration(self.next_wait_threshold_minutes?);
        let remaining = threshold.saturating_sub(self.completed_wait);
        started_at.checked_add(remaining)
    }

    /// Consume one newly crossed threshold at `now`.
    #[cfg(test)]
    pub(crate) fn take_crossed_wait_threshold_at(&mut self, now: Instant) -> Option<u32> {
        if self.phase != AgentWorkStatusPhase::Working {
            return None;
        }
        let threshold_minutes = self.next_wait_threshold_minutes?;
        let total = self
            .wait_started_at
            .map_or(self.completed_wait, |started_at| {
                self.completed_wait
                    .saturating_add(now.saturating_duration_since(started_at))
            });
        if total < wait_threshold_duration(threshold_minutes) {
            return None;
        }
        self.next_wait_threshold_minutes = next_wait_threshold(threshold_minutes);
        self.wait_catchup_deadline = self.next_wait_threshold_is_crossed_at(now).then_some(now);
        Some(threshold_minutes)
    }

    /// Capture every threshold crossed by `now` as one compact ordered range.
    pub(crate) fn take_all_crossed_wait_thresholds_at(
        &mut self,
        now: Instant,
    ) -> Option<CrossedWaitThresholds> {
        if self.phase != AgentWorkStatusPhase::Working {
            return None;
        }
        let first = self.next_wait_threshold_minutes?;
        let total_minutes = self
            .wait_started_at
            .map_or(self.completed_wait, |started_at| {
                self.completed_wait
                    .saturating_add(now.saturating_duration_since(started_at))
            })
            .as_secs()
            / 60;
        if total_minutes < u64::from(first) {
            return None;
        }

        let mut remaining = 0_u32;
        let mut cursor = Some(first);
        while let Some(threshold) = cursor.filter(|threshold| *threshold < 120) {
            if total_minutes < u64::from(threshold) {
                break;
            }
            remaining = remaining.saturating_add(1);
            cursor = next_wait_threshold(threshold);
        }
        if let Some(threshold) = cursor
            && u64::from(threshold) <= total_minutes
        {
            let additional = ((total_minutes - u64::from(threshold)) / 120)
                .saturating_add(1)
                .min(u64::from(u32::MAX));
            remaining = remaining.saturating_add(u32::try_from(additional).unwrap_or(u32::MAX));
            let successor = u64::from(threshold).saturating_add(additional.saturating_mul(120));
            cursor = u32::try_from(successor).ok();
        }
        self.next_wait_threshold_minutes = cursor;
        self.wait_catchup_deadline = None;
        Some(CrossedWaitThresholds {
            next_minutes: first,
            remaining,
        })
    }

    /// Return whether accumulated time has crossed the pending threshold.
    fn next_wait_threshold_is_crossed_at(&self, now: Instant) -> bool {
        let Some(threshold_minutes) = self.next_wait_threshold_minutes else {
            return false;
        };
        let total = self
            .wait_started_at
            .map_or(self.completed_wait, |started_at| {
                self.completed_wait
                    .saturating_add(now.saturating_duration_since(started_at))
            });
        wait_threshold_duration(threshold_minutes) <= total
    }

    /// Decide the exact post-commit behavior for one no-tool final.
    pub(crate) fn decide_final(&self, input: FinalStatusInput) -> Option<FinalStatusDecision> {
        let gated = self.phase == AgentWorkStatusPhase::Working
            || (self.phase == AgentWorkStatusPhase::Unreported && input.status_was_available);
        if !gated {
            return None;
        }
        if !input.successful {
            return (self.phase == AgentWorkStatusPhase::Working)
                .then_some(FinalStatusDecision::Accept);
        }
        if MAX_FINAL_CHALLENGES <= self.final_challenges_sent {
            return Some(FinalStatusDecision::Accept);
        }
        let challenge = match self.phase {
            AgentWorkStatusPhase::Unreported => FinalStatusChallenge::Unreported,
            AgentWorkStatusPhase::Working => FinalStatusChallenge::Working {
                title: self
                    .title
                    .clone()
                    .expect("Working status has an accepted title"),
            },
            AgentWorkStatusPhase::Done
            | AgentWorkStatusPhase::Blocked
            | AgentWorkStatusPhase::Unknown => return None,
        };
        Some(FinalStatusDecision::Challenge(challenge))
    }

    /// Record one committed challenged response if its captured unresolved
    /// phase remains current.
    pub(crate) fn record_final_challenge(&mut self, challenge: &FinalStatusChallenge) {
        if self.phase == challenge.phase() {
            self.final_challenges_sent = self.final_challenges_sent.saturating_add(1);
        }
    }

    /// Record admitted substantive tool work while the current status is not
    /// Working.
    pub(crate) fn record_substantive_tool_admission(&mut self) {
        if self.phase != AgentWorkStatusPhase::Working {
            self.working_reminder_pending = true;
        }
    }

    /// Consume the current foreground round's reminder obligation.
    pub(crate) fn take_working_reminder(&mut self) -> bool {
        std::mem::take(&mut self.working_reminder_pending)
    }

    /// Discard a reminder obligation when its foreground round is suppressed or
    /// cancelled.
    pub(crate) fn clear_working_reminder(&mut self) {
        self.working_reminder_pending = false;
    }

    /// Invalidate Working after an unsuccessful terminal or successful budget
    /// escape.
    pub(crate) fn invalidate_working(&mut self) -> bool {
        if self.phase != AgentWorkStatusPhase::Working {
            return false;
        }
        self.phase = AgentWorkStatusPhase::Unknown;
        self.wait_started_at = None;
        self.wait_catchup_deadline = None;
        true
    }
}

/// Convert one protocol threshold to its monotonic duration.
fn wait_threshold_duration(minutes: u32) -> Duration {
    Duration::from_secs(u64::from(minutes).saturating_mul(60))
}

/// Advance the approved `15, 30, 60, 120, 240, 360, ...` sequence.
fn next_wait_threshold(current: u32) -> Option<u32> {
    match current {
        15 => Some(30),
        30 => Some(60),
        60 => Some(120),
        _ => current.checked_add(120),
    }
}

#[cfg(test)]
mod tests;

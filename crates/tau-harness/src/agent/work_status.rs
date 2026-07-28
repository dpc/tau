//! Runtime-only semantic work-status state.

use tau_proto::AgentWorkStatusPhase;

/// Maximum number of committed successful finals challenged per Working epoch.
const MAX_FINAL_REMINDERS: u8 = 2;

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
            return Err("status title must not be empty".to_owned());
        }
        if 160 < canonical.len() {
            return Err("status title must be at most 160 UTF-8 bytes".to_owned());
        }
        if canonical
            .chars()
            .any(|character| character.is_control() || matches!(character, '\u{2028}' | '\u{2029}'))
        {
            return Err("status title must be one line without control characters".to_owned());
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

/// Gate decision for one no-tool final while Working.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkingFinalDecision {
    /// Retain the outer turn and continue after commit.
    Challenge,
    /// Invalidate Working and terminalize after commit.
    AcceptUnknown,
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
    /// Whether the acknowledgement notice was already scheduled.
    ack_notice_delivered: bool,
    /// Number of committed nominal final responses challenged in this epoch.
    final_reminders_sent: u8,
}

impl Default for WorkStatus {
    fn default() -> Self {
        Self {
            phase: AgentWorkStatusPhase::Unreported,
            title: None,
            epoch: 0,
            ack_notice_delivered: false,
            final_reminders_sent: 0,
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

    /// Apply a validated model report and return whether its snapshot changed.
    pub(crate) fn report(&mut self, report: WorkStatusReport) -> bool {
        let WorkStatusReport { phase, title } = report;
        if self.phase == phase && self.title.as_deref() == Some(title.as_str()) {
            if phase == AgentWorkStatusPhase::Working {
                self.ack_notice_delivered = true;
            }
            return false;
        }
        if phase == AgentWorkStatusPhase::Working && self.phase != AgentWorkStatusPhase::Working {
            self.epoch = self.epoch.saturating_add(1);
            self.final_reminders_sent = 0;
        }
        self.phase = phase;
        self.title = Some(title);
        if phase == AgentWorkStatusPhase::Working {
            self.ack_notice_delivered = true;
        }
        true
    }

    /// Decide the exact post-commit behavior for one no-tool final.
    pub(crate) fn decide_final(&self, successful: bool) -> Option<WorkingFinalDecision> {
        if self.phase != AgentWorkStatusPhase::Working {
            return None;
        }
        if !successful || MAX_FINAL_REMINDERS <= self.final_reminders_sent {
            return Some(WorkingFinalDecision::AcceptUnknown);
        }
        Some(WorkingFinalDecision::Challenge)
    }

    /// Reset acknowledgement delivery for a newly running outer turn.
    pub(crate) fn reset_ack_notice(&mut self) {
        self.ack_notice_delivered = false;
    }

    /// Mark the acknowledgement as delivered unless it was already delivered.
    pub(crate) fn mark_ack_notice_delivered(&mut self) -> bool {
        if self.ack_notice_delivered {
            return false;
        }
        self.ack_notice_delivered = true;
        true
    }

    /// Record one committed challenged response.
    pub(crate) fn record_final_challenge(&mut self) {
        self.final_reminders_sent = self.final_reminders_sent.saturating_add(1);
    }

    /// Invalidate Working after an unsuccessful terminal or exhausted challenge
    /// budget.
    pub(crate) fn invalidate_working(&mut self) -> bool {
        if self.phase != AgentWorkStatusPhase::Working {
            return false;
        }
        self.phase = AgentWorkStatusPhase::Unknown;
        true
    }
}

#[cfg(test)]
mod tests;

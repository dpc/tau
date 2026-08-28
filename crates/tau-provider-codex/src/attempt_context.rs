//! Lifecycle facts for one finite Codex provider attempt.

use crate::attempt_failure::{
    AttemptCaptureCorrelation, AttemptCaptureSnapshot, AttemptFailureEvidence, CaptureInput,
    LogicalAttempt,
};
use crate::{Prompt, SemanticProgress, StreamState};

/// Provider operation performed by one finite attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptOperation {
    /// Ordinary Responses inference.
    Inference,
    /// Native standalone Responses compaction.
    Compact,
}

impl AttemptOperation {
    /// Return the private diagnostic label for this operation.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Inference => "inference",
            Self::Compact => "compact",
        }
    }
}

/// Correlation and monotonic evidence owned by one finite provider attempt.
pub(crate) struct ProviderAttemptContext {
    /// Operation whose policy will consume the extracted evidence.
    operation: AttemptOperation,
    /// Logical-attempt and actual wire-dispatch correlation.
    correlation: AttemptCaptureCorrelation,
    /// Sticky parser-accepted semantic progress.
    progress: SemanticProgress,
    /// Whether the one permitted failure finalization already ran.
    failure_finalized: bool,
}

/// Borrowed inputs needed to finalize one retry failure.
pub(crate) struct RetryFailureInput<'a, 'prompt> {
    /// Agent prompt correlated with the attempt.
    pub(crate) agent_prompt_id: &'a str,
    /// Prompt and private-capture eligibility.
    pub(crate) request: &'a Prompt<'prompt>,
    /// Closed scheduler decision.
    pub(crate) decision: &'a tau_provider::retry_policy::RetryDecision,
    /// Structured provider or transport evidence.
    pub(crate) evidence: Option<&'a AttemptFailureEvidence>,
    /// Access token used only for exact secret rejection.
    pub(crate) access_token: &'a str,
    /// Account identifier used only for exact secret rejection.
    pub(crate) account_id: Option<&'a str>,
}

#[cfg(test)]
#[path = "attempt_context_tests.rs"]
mod tests;

impl ProviderAttemptContext {
    /// Start one operation before any provider egress.
    #[must_use]
    pub(crate) fn new(operation: AttemptOperation, logical_attempt: LogicalAttempt) -> Self {
        Self {
            operation,
            correlation: AttemptCaptureCorrelation::new(logical_attempt),
            progress: SemanticProgress::None,
            failure_finalized: false,
        }
    }

    /// Mutably borrow the wire correlation used by the transport.
    pub(crate) fn correlation(&mut self) -> &mut AttemptCaptureCorrelation {
        &mut self.correlation
    }

    /// Retain semantic progress monotonically across transparent transport
    /// work.
    pub(crate) fn observe_stream(&mut self, state: &StreamState) {
        if state.has_semantic_progress() {
            self.progress = SemanticProgress::Parsed;
        }
    }

    /// Return sticky progress observed anywhere in this attempt.
    #[must_use]
    pub(crate) fn progress(&self) -> SemanticProgress {
        if self.correlation.snapshot().semantic_progress() == SemanticProgress::Parsed {
            SemanticProgress::Parsed
        } else {
            self.progress
        }
    }

    /// Return whether an actual provider request crossed the egress boundary.
    #[must_use]
    pub(crate) fn backend_reached(&self) -> bool {
        0 < self.correlation.snapshot().wire_dispatches()
    }

    /// Return immutable attempt correlation for terminal reporting.
    #[must_use]
    pub(crate) fn snapshot(&self) -> AttemptCaptureSnapshot {
        self.correlation.snapshot()
    }

    /// Finalize one retry failure capture at most once.
    pub(crate) fn finalize_retry_failure(&mut self, input: RetryFailureInput<'_, '_>) {
        let Some(input) = self.take_retry_failure(input) else {
            return;
        };
        crate::attempt_failure::submit_capture(input);
    }

    /// Assemble the sole final failure input for production and focused tests.
    fn take_retry_failure<'a, 'prompt>(
        &mut self,
        input: RetryFailureInput<'a, 'prompt>,
    ) -> Option<CaptureInput<'a>>
    where
        'prompt: 'a,
    {
        if self.failure_finalized {
            return None;
        }
        self.failure_finalized = true;
        let progress = self.progress();
        let correlation = self.snapshot();
        Some(CaptureInput {
            operation: self.operation,
            agent_prompt_id: input.agent_prompt_id,
            request: input.request,
            decision: input.decision,
            progress,
            correlation,
            response_bytes_received: correlation.response_bytes_received(),
            evidence: input.evidence,
            access_token: input.access_token,
            account_id: input.account_id,
        })
    }
}

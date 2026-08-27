//! Content-free aggregate timing for one immediate UI prompt publication.
//!
//! The owner starts after the harness accepts an authenticated Human UI prompt
//! for immediate dispatch and before it performs dispatch setup. It follows
//! that prompt through generic interception, semantic admission/folding, debug
//! recording, and observer-bus admission. It emits exactly one terminal result
//! when publication is accepted, rejected, or released by teardown. It does not
//! enter the protocol, journals, event log, replay, or any publication
//! decision.

use std::time::{Duration, Instant};

use tau_core::DeliveryOutcomeCount;

/// Process-local timing ownership for one eligible UI prompt publication.
pub(crate) struct PromptAcceptanceTiming {
    /// Monotonic start before ordinary dispatch setup begins.
    started: Instant,
    /// Time spent in dispatch and generic-publication setup.
    setup: Duration,
    /// Time spent waiting for one or more interceptor replies.
    interception_wait: Duration,
    /// Start of the currently open interceptor wait, when any.
    interception_wait_started: Option<Instant>,
    /// Time spent synchronously processing and admitting the debug record.
    debug_record_admission: Duration,
    /// Time spent admitting and folding the semantic event.
    semantic_admission_fold: Duration,
    /// Time spent admitting the observer bus frame.
    bus_admission: Duration,
    /// Number of session-metadata precursor observations in this timing window.
    precursor_stats_count: u8,
    /// Eligible observer delivery outcomes at bus admission.
    eligible_delivery_count: DeliveryOutcomeCount,
    /// Whether an explicit terminal result consumed this timing owner.
    resolved: bool,
}

/// Closed terminal classes for aggregate UI prompt-publication timing.
pub(super) enum PromptAcceptanceTerminal {
    /// The prompt reached semantic and observer-bus acceptance.
    Accepted,
    /// Semantic admission rejected the prompt.
    SemanticAdmissionRejected,
    /// Teardown or an uninstrumented early return released the timing owner.
    StaleOrTornDown,
}

impl PromptAcceptanceTerminal {
    /// Returns the fixed content-free trace classification for this terminal.
    const fn name(self) -> &'static str {
        match self {
            Self::Accepted => "publication_accepted",
            Self::SemanticAdmissionRejected => "semantic_admission_rejected",
            Self::StaleOrTornDown => "stale_or_torn_down",
        }
    }
}

impl PromptAcceptanceTiming {
    /// Starts timing after activation observation and before dispatch setup.
    pub(super) fn new() -> Self {
        Self {
            started: Instant::now(),
            setup: Duration::ZERO,
            interception_wait: Duration::ZERO,
            interception_wait_started: None,
            debug_record_admission: Duration::ZERO,
            semantic_admission_fold: Duration::ZERO,
            bus_admission: Duration::ZERO,
            precursor_stats_count: 0,
            eligible_delivery_count: DeliveryOutcomeCount::default(),
            resolved: false,
        }
    }

    /// Counts the session-metadata precursor observation in this timing window.
    pub(super) fn note_precursor_stats(&mut self) {
        self.precursor_stats_count = self.precursor_stats_count.saturating_add(1);
    }

    /// Ends the setup interval when publication enters generic interception.
    pub(super) fn finish_setup(&mut self) {
        self.setup = self.elapsed();
    }

    /// Starts or resumes time parked behind generic interception.
    pub(super) fn begin_interception_wait(&mut self) {
        self.interception_wait_started = Some(Instant::now());
    }

    /// Stops time parked behind generic interception.
    pub(super) fn end_interception_wait(&mut self) {
        if let Some(started) = self.interception_wait_started.take() {
            self.interception_wait = self.interception_wait.saturating_add(started.elapsed());
        }
    }

    /// Records the existing semantic admission-and-fold phase.
    pub(super) fn set_semantic_admission_fold(&mut self, duration: Duration) {
        self.semantic_admission_fold = duration;
    }

    /// Records synchronous debug-record processing and queue admission.
    pub(super) fn set_debug_record_admission(&mut self, duration: Duration) {
        self.debug_record_admission = duration;
    }

    /// Records bus admission and its eligible delivery outcomes.
    pub(super) fn set_bus_admission(
        &mut self,
        duration: Duration,
        eligible_delivery_count: DeliveryOutcomeCount,
    ) {
        self.bus_admission = duration;
        self.eligible_delivery_count = eligible_delivery_count;
    }

    /// Emits one terminal aggregate timing result.
    pub(super) fn finish(mut self, terminal: PromptAcceptanceTerminal) {
        self.end_interception_wait();
        self.emit(terminal);
        self.resolved = true;
    }

    /// Returns elapsed monotonic time since the UI dispatch began.
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Emits the fixed content-free aggregate trace fields.
    fn emit(&self, terminal: PromptAcceptanceTerminal) {
        let total = self.elapsed();
        let measured = self
            .setup
            .saturating_add(self.interception_wait)
            .saturating_add(self.debug_record_admission)
            .saturating_add(self.semantic_admission_fold)
            .saturating_add(self.bus_admission);
        tracing::event!(
            target: "tau_harness::prompt_acceptance",
            tracing::Level::INFO,
            result_class = terminal.name(),
            ui_dispatch_to_publication_acceptance_us = duration_us(total),
            setup_us = duration_us(self.setup),
            interception_wait_us = duration_us(self.interception_wait),
            debug_record_admission_us = duration_us(self.debug_record_admission),
            semantic_admission_fold_us = duration_us(self.semantic_admission_fold),
            bus_admission_us = duration_us(self.bus_admission),
            residual_us = duration_us(total.saturating_sub(measured)),
            precursor_stats_count = self.precursor_stats_count,
            eligible_connection_count = self.eligible_delivery_count.get(),
        );
    }
}

impl Drop for PromptAcceptanceTiming {
    fn drop(&mut self) {
        if !self.resolved {
            self.end_interception_wait();
            self.emit(PromptAcceptanceTerminal::StaleOrTornDown);
        }
    }
}

/// Converts a duration to the bounded microsecond unit used by trace fields.
fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

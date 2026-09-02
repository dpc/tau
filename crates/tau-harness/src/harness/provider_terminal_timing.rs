//! Private, content-free timing for the provider terminal pipeline.
//!
//! This module deliberately owns no event, persistence, or extension identity.
//! It is enabled only by its trace target and emits one fixed-shape record for
//! each accepted terminal that reaches projection.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tau_proto::Event;

/// Trace target for provider-terminal subphase measurements.
pub(crate) const TRACE_TARGET: &str = "tau_harness::provider_terminal_timing";

const STAGE_COUNT: usize = 9;
static NEXT_CORRELATION: AtomicU64 = AtomicU64::new(1);

/// Non-overlapping source-order phases in the terminal pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderTerminalStage {
    /// Aggregates output items before semantic mutation.
    Projection,
    /// Validates and normalizes the report before shared accounting.
    Normalize,
    /// Attaches usage, telemetry, and accounting state.
    Accounting,
    /// Selects the terminal family and retained reducer plan.
    Classification,
    /// Copies a response into a retained commit-gated terminal plan.
    RetainedPlanClone,
    /// Copies the response into the canonical publication candidate.
    CanonicalCandidateClone,
    /// Offers the canonical candidate to publication and interception.
    CanonicalEnqueue,
    /// Runs eager non-commit-gated terminal reducers.
    EagerReducer,
    /// Runs the reducer held behind a successful canonical commit.
    CommitGatedReducer,
}

impl ProviderTerminalStage {
    const fn index(self) -> usize {
        match self {
            Self::Projection => 0,
            Self::Normalize => 1,
            Self::Accounting => 2,
            Self::Classification => 3,
            Self::RetainedPlanClone => 4,
            Self::CanonicalCandidateClone => 5,
            Self::CanonicalEnqueue => 6,
            Self::EagerReducer => 7,
            Self::CommitGatedReducer => 8,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Projection => "projection",
            Self::Normalize => "normalize",
            Self::Accounting => "accounting",
            Self::Classification => "classification",
            Self::RetainedPlanClone => "retained_plan_clone",
            Self::CanonicalCandidateClone => "canonical_candidate_clone",
            Self::CanonicalEnqueue => "canonical_enqueue",
            Self::EagerReducer => "eager_reducer",
            Self::CommitGatedReducer => "commit_gated_reducer",
        }
    }
}

/// Process-local owner for at most one accepted terminal timing sample.
#[derive(Default)]
pub(crate) struct ProviderTerminalTiming {
    /// Active terminal sample; the harness processes terminal reports serially.
    active: Option<Rc<RefCell<ProviderTerminalTimingSample>>>,
    /// Test-only completed samples for deterministic source-order assertions.
    #[cfg(test)]
    completed: Vec<ProviderTerminalTimingSnapshot>,
    /// Test-only opt-in that exercises the production measurement path.
    #[cfg(test)]
    enabled_for_test: bool,
}

impl ProviderTerminalTiming {
    /// Starts a sample after current-owner admission and immediately before
    /// projection.
    pub(crate) fn start_accepted_terminal(&mut self) {
        if self.active.is_some() {
            panic!("accepted provider terminal timing must not overlap");
        }
        if !self.is_enabled() {
            return;
        }
        self.active = Some(Rc::new(RefCell::new(ProviderTerminalTimingSample::new(
            NEXT_CORRELATION.fetch_add(1, Ordering::Relaxed),
        ))));
    }

    /// Returns whether this accepted terminal has enabled process-local timing.
    pub(crate) const fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Starts one source-order stage when timing is enabled.
    pub(crate) fn start_stage(&mut self, stage: ProviderTerminalStage) {
        let Some(sample) = &self.active else {
            return;
        };
        sample.borrow_mut().start_stage(stage);
    }

    /// Declares one stage applicable before its source boundary executes.
    pub(crate) fn require_stage(&mut self, stage: ProviderTerminalStage) {
        let Some(sample) = &self.active else {
            return;
        };
        sample.borrow_mut().applicable[stage.index()] = true;
    }

    /// Finishes one source-order stage when timing is enabled.
    pub(crate) fn finish_stage(&mut self, stage: ProviderTerminalStage) {
        let Some(sample) = &self.active else {
            return;
        };
        sample.borrow_mut().finish_stage(stage);
    }

    /// Finishes a stage only when it remains open after an optional nested
    /// commit.
    pub(crate) fn finish_stage_if_open(&mut self, stage: ProviderTerminalStage) {
        let Some(sample) = &self.active else {
            return;
        };
        if sample
            .borrow()
            .open_stage
            .is_some_and(|(open, _)| open == stage)
        {
            sample.borrow_mut().finish_stage(stage);
        }
    }

    /// Ends enqueue timing at the precise entry to the immediately nested
    /// canonical commit.
    pub(crate) fn finish_canonical_enqueue_at_commit_entry(&mut self, event: &Event) {
        if matches!(event, Event::ProviderResponseFinished(_)) {
            self.finish_stage_if_open(ProviderTerminalStage::CanonicalEnqueue);
        }
    }

    /// Records deterministic work performed on a response without retaining its
    /// content.
    pub(crate) fn record_work(
        &mut self,
        output_items_visited: usize,
        tool_calls_visited: usize,
        response_bytes_visited: usize,
    ) {
        let Some(sample) = &self.active else {
            return;
        };
        let mut sample = sample.borrow_mut();
        sample.output_items_visited = output_items_visited;
        sample.tool_calls_visited = tool_calls_visited;
        sample.response_bytes_visited = response_bytes_visited;
    }

    /// Records bytes copied by one full-response clone.
    pub(crate) fn record_response_clone(
        &mut self,
        output_items: usize,
        tool_calls: usize,
        response_bytes: usize,
    ) {
        let Some(sample) = &self.active else {
            return;
        };
        let mut sample = sample.borrow_mut();
        sample.output_items_copied = sample.output_items_copied.saturating_add(output_items);
        sample.tool_calls_copied = sample.tool_calls_copied.saturating_add(tool_calls);
        sample.response_bytes_copied = sample.response_bytes_copied.saturating_add(response_bytes);
    }

    /// Returns correlation state for the immediately nested canonical commit.
    pub(crate) fn canonical_commit_timing(
        &self,
        event: &Event,
    ) -> Option<ProviderTerminalCommitTiming> {
        if !matches!(event, Event::ProviderResponseFinished(_)) {
            return None;
        }
        let sample = self.active.as_ref()?.clone();
        if !sample.borrow().canonical_enqueue_started {
            return None;
        }
        Some(ProviderTerminalCommitTiming { sample })
    }

    /// Completes and emits the fixed-cardinality timing record.
    pub(crate) fn finish_accepted_terminal(&mut self) {
        let Some(sample) = self.active.take() else {
            return;
        };
        let snapshot = sample.borrow_mut().finish();
        tracing::trace!(
            target: TRACE_TARGET,
            correlation = snapshot.correlation,
            projection_us = snapshot.stage_us(ProviderTerminalStage::Projection),
            normalize_us = snapshot.stage_us(ProviderTerminalStage::Normalize),
            accounting_us = snapshot.stage_us(ProviderTerminalStage::Accounting),
            classification_us = snapshot.stage_us(ProviderTerminalStage::Classification),
            retained_plan_clone_us = snapshot.stage_us(ProviderTerminalStage::RetainedPlanClone),
            canonical_candidate_clone_us = snapshot.stage_us(ProviderTerminalStage::CanonicalCandidateClone),
            canonical_enqueue_us = snapshot.stage_us(ProviderTerminalStage::CanonicalEnqueue),
            eager_reducer_us = snapshot.stage_us(ProviderTerminalStage::EagerReducer),
            commit_gated_reducer_us = snapshot.stage_us(ProviderTerminalStage::CommitGatedReducer),
            canonical_commit_present = snapshot.canonical_commit.is_some(),
            canonical_commit_us = snapshot.canonical_commit.unwrap_or(Duration::ZERO).as_micros(),
            pipeline_total_us = snapshot.pipeline_total.as_micros(),
            unattributed_us = snapshot.unattributed.as_micros(),
            output_items_visited = snapshot.output_items_visited,
            tool_calls_visited = snapshot.tool_calls_visited,
            response_bytes_visited = snapshot.response_bytes_visited,
            output_items_copied = snapshot.output_items_copied,
            tool_calls_copied = snapshot.tool_calls_copied,
            response_bytes_copied = snapshot.response_bytes_copied,
            "provider terminal subphase timing"
        );
        #[cfg(test)]
        self.completed.push(snapshot);
    }

    /// Enables deterministic measurement tests without changing production
    /// configuration.
    #[cfg(test)]
    pub(crate) fn enable_for_test(&mut self) {
        self.enabled_for_test = true;
    }

    /// Returns and clears deterministic completed samples.
    #[cfg(test)]
    pub(crate) fn take_completed_for_test(&mut self) -> Vec<ProviderTerminalTimingSnapshot> {
        std::mem::take(&mut self.completed)
    }

    fn is_enabled(&self) -> bool {
        tracing::enabled!(target: TRACE_TARGET, tracing::Level::TRACE)
            || cfg!(test) && {
                #[cfg(test)]
                {
                    self.enabled_for_test
                }
                #[cfg(not(test))]
                {
                    false
                }
            }
    }
}

/// Correlation owned by `CommitEventTiming` while a canonical terminal commits.
pub(crate) struct ProviderTerminalCommitTiming {
    /// Shared process-local sample that owns the canonical total.
    sample: Rc<RefCell<ProviderTerminalTimingSample>>,
}

impl ProviderTerminalCommitTiming {
    /// Records the existing canonical commit total without changing commit
    /// behavior.
    pub(crate) fn finish(self, total: Duration) {
        let mut sample = self.sample.borrow_mut();
        if sample.canonical_commit.replace(total).is_some() {
            panic!("provider terminal canonical commit timing repeated");
        }
    }
}

/// Test-visible, content-free completed timing record.
#[derive(Clone, Debug)]
pub(crate) struct ProviderTerminalTimingSnapshot {
    /// Process-local correlation token.
    pub(crate) correlation: u64,
    /// Completed duration for each source-order stage.
    stages: [Option<Duration>; STAGE_COUNT],
    /// Exact source-order completion sequence for deterministic assertions.
    #[cfg(test)]
    pub(crate) stage_order: Vec<ProviderTerminalStage>,
    /// Existing canonical commit duration, when publication was not parked.
    pub(crate) canonical_commit: Option<Duration>,
    /// Total accepted terminal pipeline duration.
    pub(crate) pipeline_total: Duration,
    /// Pipeline duration outside the non-overlapping recorded stages.
    pub(crate) unattributed: Duration,
    /// Number of response items visited during projection.
    pub(crate) output_items_visited: usize,
    /// Number of tool calls visited during projection.
    pub(crate) tool_calls_visited: usize,
    /// Content-free response bytes visited by the fixture.
    pub(crate) response_bytes_visited: usize,
    /// Number of output items copied into retained response owners.
    pub(crate) output_items_copied: usize,
    /// Number of tool calls copied into retained response owners.
    pub(crate) tool_calls_copied: usize,
    /// Content-free response bytes copied into retained owners.
    pub(crate) response_bytes_copied: usize,
}

impl ProviderTerminalTimingSnapshot {
    /// Returns a phase duration for deterministic test assertions.
    pub(crate) fn stage(&self, stage: ProviderTerminalStage) -> Option<Duration> {
        self.stages[stage.index()]
    }

    fn stage_us(&self, stage: ProviderTerminalStage) -> u128 {
        self.stage(stage).unwrap_or(Duration::ZERO).as_micros()
    }
}

/// Mutable state for a single content-free terminal timing record.
struct ProviderTerminalTimingSample {
    /// Process-local correlation token.
    correlation: u64,
    /// Start of terminal processing immediately before projection.
    started: Instant,
    /// Duration of each source-order stage.
    stages: [Option<Duration>; STAGE_COUNT],
    /// Stages selected by the terminal family before their source boundary.
    applicable: [bool; STAGE_COUNT],
    /// Currently open stage, which prevents overlap and repetition.
    open_stage: Option<(ProviderTerminalStage, Instant)>,
    /// Exact source-order completion sequence.
    #[cfg(test)]
    stage_order: Vec<ProviderTerminalStage>,
    /// Whether canonical enqueue has begun for this sample.
    canonical_enqueue_started: bool,
    /// Existing canonical `commit_event` total when it entered immediately.
    canonical_commit: Option<Duration>,
    /// Deterministic content-free work counters.
    output_items_visited: usize,
    tool_calls_visited: usize,
    response_bytes_visited: usize,
    output_items_copied: usize,
    tool_calls_copied: usize,
    response_bytes_copied: usize,
}

impl ProviderTerminalTimingSample {
    fn new(correlation: u64) -> Self {
        Self {
            correlation,
            started: Instant::now(),
            stages: [None; STAGE_COUNT],
            applicable: [false; STAGE_COUNT],
            open_stage: None,
            #[cfg(test)]
            stage_order: Vec::with_capacity(STAGE_COUNT),
            canonical_enqueue_started: false,
            canonical_commit: None,
            output_items_visited: 0,
            tool_calls_visited: 0,
            response_bytes_visited: 0,
            output_items_copied: 0,
            tool_calls_copied: 0,
            response_bytes_copied: 0,
        }
    }

    fn start_stage(&mut self, stage: ProviderTerminalStage) {
        if self.open_stage.is_some() {
            panic!(
                "provider terminal timing stage {} overlaps another stage",
                stage.name()
            );
        }
        if self.stages[stage.index()].is_some() {
            panic!("provider terminal timing stage {} repeated", stage.name());
        }
        self.applicable[stage.index()] = true;
        if self
            .stages
            .iter()
            .enumerate()
            .any(|(index, duration)| index > stage.index() && duration.is_some())
        {
            panic!(
                "provider terminal timing stage {} violates source order",
                stage.name()
            );
        }
        if stage == ProviderTerminalStage::CanonicalEnqueue {
            self.canonical_enqueue_started = true;
        }
        self.open_stage = Some((stage, Instant::now()));
    }

    fn finish_stage(&mut self, stage: ProviderTerminalStage) {
        let Some((open, started)) = self.open_stage.take() else {
            panic!(
                "provider terminal timing stage {} was never started",
                stage.name()
            );
        };
        if open != stage {
            panic!(
                "provider terminal timing stage {} finished while {} is open",
                stage.name(),
                open.name()
            );
        }
        self.stages[stage.index()] = Some(started.elapsed());
        #[cfg(test)]
        self.stage_order.push(stage);
    }

    fn finish(&mut self) -> ProviderTerminalTimingSnapshot {
        if let Some((stage, _)) = self.open_stage {
            panic!(
                "provider terminal timing stage {} was omitted",
                stage.name()
            );
        }
        for (index, applicable) in self.applicable.iter().copied().enumerate() {
            if applicable && self.stages[index].is_none() {
                panic!("provider terminal timing applicable stage {index} was omitted");
            }
        }
        let pipeline_total = self.started.elapsed();
        let attributed = self
            .stages
            .iter()
            .flatten()
            .copied()
            .fold(Duration::ZERO, Duration::saturating_add);
        let unattributed = pipeline_total.saturating_sub(attributed);
        ProviderTerminalTimingSnapshot {
            correlation: self.correlation,
            stages: self.stages,
            #[cfg(test)]
            stage_order: std::mem::take(&mut self.stage_order),
            canonical_commit: self.canonical_commit,
            pipeline_total,
            unattributed,
            output_items_visited: self.output_items_visited,
            tool_calls_visited: self.tool_calls_visited,
            response_bytes_visited: self.response_bytes_visited,
            output_items_copied: self.output_items_copied,
            tool_calls_copied: self.tool_calls_copied,
            response_bytes_copied: self.response_bytes_copied,
        }
    }
}

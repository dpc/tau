//! Content-free, process-local prompt-materialization diagnostics.
//!
//! The diagnostic is enabled only when its tracing target is active. It owns no
//! prompt data and records only fixed-cardinality durations and bounded scalar
//! counts. It never participates in dispatch, persistence, or retry decisions.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    static DIAGNOSTIC_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Start one diagnostic-only clock without doing work while diagnostics are
/// disabled.
pub(super) fn stage_start(timing: Option<&PromptMaterializationTiming>) -> Option<Instant> {
    timing.map(|_| {
        #[cfg(test)]
        DIAGNOSTIC_WORK.set(DIAGNOSTIC_WORK.get() + 1);
        Instant::now()
    })
}

/// Reset the exact diagnostic-work oracle for the current test thread.
#[cfg(test)]
pub(super) fn reset_diagnostic_work() {
    DIAGNOSTIC_WORK.set(0);
}

/// Return diagnostic work performed on the current test thread.
#[cfg(test)]
pub(super) fn diagnostic_work() -> usize {
    DIAGNOSTIC_WORK.get()
}

/// Account diagnostic-only count/schema traversal in deterministic tests.
#[cfg(test)]
pub(super) fn note_count_work() {
    DIAGNOSTIC_WORK.set(DIAGNOSTIC_WORK.get() + 1);
}

/// Fixed stage order for one checkpoint-authorized provider prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MaterializationStage {
    /// Assemble the selected branch and provider context.
    BranchContext,
    /// Resolve tool definitions and inspect schema sizes.
    ToolsSchema,
    /// Resolve prompt fragments, skills, and dynamic template context.
    FragmentsSkillsContext,
    /// Render the strict Handlebars prompt.
    HandlebarsRender,
    /// Install process-local request accounting.
    Accounting,
    /// Copy or fan out the transient request to its bus recipients.
    CopyFanout,
}

impl MaterializationStage {
    /// Return this stage's stable position in the emitted schema.
    pub(super) const fn index(self) -> usize {
        match self {
            Self::BranchContext => 0,
            Self::ToolsSchema => 1,
            Self::FragmentsSkillsContext => 2,
            Self::HandlebarsRender => 3,
            Self::Accounting => 4,
            Self::CopyFanout => 5,
        }
    }
}

/// Independent fields measured before the durable checkpoint commits.
#[derive(Clone, Default)]
pub(crate) struct PrecheckpointMaterializationTiming {
    /// Runnable scheduler selection.
    runnable_selection: Duration,
    /// Existing prompt-render preflight.
    preflight: Duration,
}

impl PrecheckpointMaterializationTiming {
    /// Construct only when the dedicated target is enabled.
    pub(crate) fn enabled() -> Option<Self> {
        tracing::enabled!(
            target: "tau_harness::prompt_materialization",
            tracing::Level::INFO
        )
        .then(Self::default)
    }

    /// Record scheduler selection without retaining its selected identifier.
    pub(crate) fn set_runnable_selection(&mut self, duration: Duration) {
        self.runnable_selection = duration;
    }

    /// Record preflight without retaining rendered content.
    pub(crate) fn set_preflight(&mut self, duration: Duration) {
        self.preflight = duration;
    }
}

/// Bounded, content-free cardinalities associated with materialization.
#[derive(Clone, Copy, Default)]
pub(super) struct MaterializationCounts {
    /// Provider-visible tool definitions.
    pub(super) tools: usize,
    /// Serialized JSON-schema bytes.
    pub(super) schema_bytes: usize,
    /// Provider context blocks.
    pub(super) context_blocks: usize,
    /// Provider context items.
    pub(super) context_items: usize,
    /// Typed image items, without bytes or metadata.
    pub(super) images: usize,
    /// Prompt recipients admitted by observer/provider fanout.
    pub(super) recipients: usize,
}

/// Shareable timing handle carried by the two one-shot publication phases.
#[derive(Clone)]
pub(crate) struct PromptMaterializationTiming {
    /// Shared content-free measurements.
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// Start immediately after the inference checkpoint commits.
    started: Instant,
    /// Disjoint stage durations in fixed schema order.
    stages: [Duration; 6],
    /// Next stage index allowed to record.
    next_stage: usize,
    /// Bounded scalar counts.
    counts: MaterializationCounts,
    /// Measurements outside the post-checkpoint subtotal.
    precheckpoint: PrecheckpointMaterializationTiming,
    /// Whether a terminal trace has already been emitted.
    resolved: bool,
}

impl PromptMaterializationTiming {
    /// Start exactly after checkpoint commit, carrying earlier independent
    /// fields only when tracing was enabled at runnable selection.
    pub(super) fn after_checkpoint(
        precheckpoint: Option<PrecheckpointMaterializationTiming>,
    ) -> Option<Self> {
        tracing::enabled!(
            target: "tau_harness::prompt_materialization",
            tracing::Level::INFO
        )
        .then(|| Self {
            inner: Arc::new(Mutex::new(Inner {
                started: Instant::now(),
                stages: [Duration::ZERO; 6],
                next_stage: 0,
                counts: MaterializationCounts::default(),
                precheckpoint: precheckpoint.unwrap_or_default(),
                resolved: false,
            })),
        })
    }

    /// Record one disjoint stage duration in the stable stage order.
    pub(super) fn record(&self, stage: MaterializationStage, duration: Duration) {
        let mut inner = self.inner.lock().expect("materialization timing lock");
        debug_assert_eq!(inner.next_stage, stage.index());
        inner.stages[stage.index()] = duration;
        inner.next_stage = stage.index() + 1;
    }

    /// Replace the current bounded count snapshot.
    pub(super) fn set_counts(&self, counts: MaterializationCounts) {
        self.inner
            .lock()
            .expect("materialization timing lock")
            .counts = counts;
    }

    /// Record the bounded number of admitted prompt recipients.
    pub(super) fn set_recipients(&self, recipients: usize) {
        self.inner
            .lock()
            .expect("materialization timing lock")
            .counts
            .recipients = recipients;
    }

    /// Finish successfully after provider/observer bus admission.
    pub(super) fn finish_success(&self) {
        self.finish("bus_admitted");
    }

    /// Finish an unsuccessful or stale materialization attempt.
    pub(super) fn finish_failure(&self) {
        self.finish("failed_or_stale");
    }

    fn finish(&self, result_class: &'static str) {
        let mut inner = self.inner.lock().expect("materialization timing lock");
        if inner.resolved {
            return;
        }
        inner.resolved = true;
        let [branch, tools, fragments, render, accounting, fanout] = inner.stages;
        let measured = inner
            .stages
            .into_iter()
            .fold(Duration::ZERO, Duration::saturating_add);
        let total = inner.started.elapsed();
        let counts = inner.counts;
        let precheckpoint = inner.precheckpoint.clone();
        tracing::event!(
            target: "tau_harness::prompt_materialization",
            tracing::Level::INFO,
            result_class,
            checkpoint_to_bus_stage_total_us = duration_us(measured),
            checkpoint_to_bus_elapsed_us = duration_us(total),
            runnable_selection_us = duration_us(precheckpoint.runnable_selection),
            preflight_us = duration_us(precheckpoint.preflight),
            branch_context_us = duration_us(branch),
            tools_schema_us = duration_us(tools),
            fragments_skills_context_us = duration_us(fragments),
            handlebars_render_us = duration_us(render),
            accounting_us = duration_us(accounting),
            copy_fanout_us = duration_us(fanout),
            residual_us = duration_us(total.saturating_sub(measured)),
            tool_count = bounded(counts.tools),
            schema_bytes = bounded(counts.schema_bytes),
            context_block_count = bounded(counts.context_blocks),
            context_item_count = bounded(counts.context_items),
            image_count = bounded(counts.images),
            recipient_count = bounded(counts.recipients),
            "content-free prompt materialization timing"
        );
    }
}

impl Drop for PromptMaterializationTiming {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1
            && self.inner.lock().is_ok_and(|inner| !inner.resolved)
        {
            self.finish_failure();
        }
    }
}

fn duration_us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn bounded(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

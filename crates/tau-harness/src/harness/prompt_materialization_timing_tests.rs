use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::prompt_materialization_timing::*;

#[derive(Clone)]
struct TraceWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("trace lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Disabled diagnostics must allocate no timing owner or materialize any
/// prompt-derived count.
#[test]
fn disabled_target_constructs_no_timing_owner() {
    assert!(PrecheckpointMaterializationTiming::enabled().is_none());
}

/// The fixed stage enum must remain contiguous and ordered so trace
/// dimensions cannot gain dynamic cardinality.
#[test]
fn stage_schema_is_exact_and_fixed() {
    let stages = [
        MaterializationStage::BranchContext,
        MaterializationStage::ToolsSchema,
        MaterializationStage::FragmentsSkillsContext,
        MaterializationStage::HandlebarsRender,
        MaterializationStage::Accounting,
        MaterializationStage::CopyFanout,
    ];
    assert_eq!(stages.map(MaterializationStage::index), [0, 1, 2, 3, 4, 5]);
}

/// The enabled diagnostic emits only fixed scalars, keeps pre-checkpoint
/// fields outside the exact post-checkpoint sum, and excludes every class
/// of sensitive production input.
#[test]
fn enabled_trace_is_content_free_and_post_checkpoint_total_is_disjoint() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&bytes);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .without_time()
        .with_ansi(false)
        .with_writer(move || TraceWriter(Arc::clone(&writer)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut pre = PrecheckpointMaterializationTiming::enabled().expect("enabled");
        pre.set_runnable_selection(Duration::from_micros(11));
        pre.set_preflight(Duration::from_micros(13));
        let timing = PromptMaterializationTiming::after_checkpoint(Some(pre)).expect("timing");
        for (stage, micros) in [
            (MaterializationStage::BranchContext, 17),
            (MaterializationStage::ToolsSchema, 19),
            (MaterializationStage::FragmentsSkillsContext, 23),
            (MaterializationStage::HandlebarsRender, 29),
            (MaterializationStage::Accounting, 31),
            (MaterializationStage::CopyFanout, 37),
        ] {
            timing.record(stage, Duration::from_micros(micros));
        }
        timing.set_counts(MaterializationCounts {
            tools: 2,
            schema_bytes: 101,
            context_blocks: 3,
            context_items: 5,
            images: 1,
            recipients: 1,
        });
        std::hint::black_box([
            "PROMPT_CANARY",
            "SCHEMA_CANARY",
            "RAW_ARGS_CANARY",
            "IMAGE_CANARY",
            "/PATH/CANARY",
            "SECRET_CANARY",
            "PROVIDER_CANARY",
            "IDENTIFIER_CANARY",
        ]);
        timing.finish_success();
    });
    let trace = String::from_utf8(bytes.lock().expect("trace lock").clone()).expect("UTF-8 trace");
    assert!(trace.contains("checkpoint_to_bus_stage_total_us=156"));
    assert!(trace.contains("runnable_selection_us=11"));
    assert!(trace.contains("preflight_us=13"));
    assert!(trace.contains("branch_context_us=17"));
    assert!(trace.contains("copy_fanout_us=37"));
    for canary in [
        "PROMPT_CANARY",
        "SCHEMA_CANARY",
        "RAW_ARGS_CANARY",
        "IMAGE_CANARY",
        "/PATH/CANARY",
        "SECRET_CANARY",
        "PROVIDER_CANARY",
        "IDENTIFIER_CANARY",
    ] {
        assert!(!trace.contains(canary), "leaked {canary}");
    }
}

/// Dropping incomplete work emits one failure record with only completed
/// disjoint work and no wall-clock threshold assertion.
#[test]
fn incomplete_attempt_emits_one_failure_without_timing_thresholds() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&bytes);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .without_time()
        .with_ansi(false)
        .with_writer(move || TraceWriter(Arc::clone(&writer)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let pre = PrecheckpointMaterializationTiming::enabled().expect("enabled");
        let timing = PromptMaterializationTiming::after_checkpoint(Some(pre)).expect("timing");
        timing.record(
            MaterializationStage::BranchContext,
            Duration::from_micros(7),
        );
    });
    let trace = String::from_utf8(bytes.lock().expect("trace lock").clone()).expect("UTF-8 trace");
    assert_eq!(trace.matches("failed_or_stale").count(), 1);
    assert!(trace.contains("checkpoint_to_bus_stage_total_us=7"));
}

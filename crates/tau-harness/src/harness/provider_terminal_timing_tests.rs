//! Deterministic unit coverage for private provider-terminal timing accounting.

use super::provider_terminal_timing::{ProviderTerminalStage, ProviderTerminalTiming};

/// Ensures a terminal family cannot declare a phase applicable and then
/// silently emit a partial timing record without that phase.
#[test]
#[should_panic(expected = "applicable stage")]
fn applicable_stage_must_complete_before_terminal_finishes() {
    let mut timing = ProviderTerminalTiming::default();
    timing.enable_for_test();
    timing.start_accepted_terminal();
    timing.require_stage(ProviderTerminalStage::Classification);
    timing.finish_accepted_terminal();
}

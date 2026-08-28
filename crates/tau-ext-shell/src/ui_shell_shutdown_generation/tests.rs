use super::*;

/// Ensures scheduled UI shell shutdown authority retains atomic wrapping
/// overflow rather than changing max-value teardown behavior.
#[test]
fn shutdown_generation_wraps_on_advance() {
    let generation_counter = UiShellShutdownGenerationCounter::new(u64::MAX);

    generation_counter.advance();

    assert_eq!(
        generation_counter.current(),
        UiShellShutdownGeneration::new(0)
    );
}

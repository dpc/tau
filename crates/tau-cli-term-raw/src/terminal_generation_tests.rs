//! Focused overflow coverage for terminal generation authorities.

use crate::presentation_mutation_generation::PresentationMutationGeneration;
use crate::redraw_sync_generation::RedrawSyncGeneration;
use crate::terminal_history_generation::TerminalHistoryGeneration;

/// History mutations must retain the existing wrapping authority boundary.
#[test]
fn terminal_history_generation_wraps_at_maximum() {
    let mut generation = TerminalHistoryGeneration::new(u64::MAX);

    generation.advance();

    assert_eq!(generation, TerminalHistoryGeneration::default());
}

/// Presentation mutations must retain the existing wrapping authority boundary.
#[test]
fn presentation_mutation_generation_wraps_at_maximum() {
    let mut generation = PresentationMutationGeneration::new(u64::MAX);

    generation.advance();

    assert_eq!(generation, PresentationMutationGeneration::default());
}

/// Synchronous redraw requests must retain the existing debug overflow trap.
#[test]
#[cfg(debug_assertions)]
#[should_panic]
fn redraw_sync_generation_panics_at_maximum_in_debug_builds() {
    let mut generation = RedrawSyncGeneration::new(u64::MAX);

    generation.advance();
}

/// Synchronous redraw requests must retain the existing unchecked wrapping
/// behavior outside debug builds.
#[test]
#[cfg(not(debug_assertions))]
fn redraw_sync_generation_wraps_at_maximum_without_debug_assertions() {
    let mut generation = RedrawSyncGeneration::new(u64::MAX);

    generation.advance();

    assert_eq!(generation, RedrawSyncGeneration::default());
}

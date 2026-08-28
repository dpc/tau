//! Shutdown authority for scheduled UI shell commands.

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-local shutdown generation captured by one scheduled UI shell
/// command.
///
/// A command whose captured generation differs from the current generation must
/// cancel before executing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UiShellShutdownGeneration(
    /// Process-local atomic-owner counter snapshot.
    u64,
);

impl UiShellShutdownGeneration {
    /// Construct a generation for a test-owned scheduler fixture.
    #[cfg(test)]
    #[must_use]
    const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Atomic owner of the shutdown authority for scheduled UI shell commands.
#[derive(Debug, Default)]
pub(super) struct UiShellShutdownGenerationCounter(
    /// Process-local scalar counter updated when the shell runtime shuts down.
    AtomicU64,
);

impl UiShellShutdownGenerationCounter {
    /// Return the generation currently authorizing scheduled UI shell commands.
    #[must_use]
    pub(super) fn current(&self) -> UiShellShutdownGeneration {
        UiShellShutdownGeneration(self.0.load(Ordering::SeqCst))
    }

    /// Advance scheduled UI shell shutdown authority with atomic wrapping
    /// overflow.
    pub(super) fn advance(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    /// Construct an atomic owner for a test-owned scheduler fixture.
    #[cfg(test)]
    #[must_use]
    const fn new(value: u64) -> Self {
        Self(AtomicU64::new(value))
    }
}

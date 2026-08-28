//! Process-local authority for redraw-synchronization waiters.

/// Identifies the redraw completion a synchronous terminal caller awaits.
///
/// This authority starts at zero and retains the ordinary-add overflow behavior
/// of synchronous redraw requests. It is never serialized.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct RedrawSyncGeneration(
    /// Process-local scalar retained by the terminal redraw owner.
    u64,
);

impl RedrawSyncGeneration {
    /// Creates a redraw synchronization authority from the owning terminal
    /// state's raw value.
    #[cfg(test)]
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Advances redraw synchronization authority with the existing ordinary-add
    /// overflow policy.
    pub(super) fn advance(&mut self) {
        self.0 += 1;
    }
}

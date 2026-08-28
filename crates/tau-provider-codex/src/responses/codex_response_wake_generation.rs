//! Process-local wake authority for compact Responses transport waiters.

/// Identifies compact Responses transport state observed by one parked waiter.
///
/// The compact transport owner starts this authority at zero and advances it
/// with wrapping arithmetic on every permit release or abort wake. It is never
/// serialized or used as a provider attempt, output, or wire identifier.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct CodexResponseWakeGeneration(
    /// Process-local scalar retained by compact transport state.
    u64,
);

impl CodexResponseWakeGeneration {
    /// Advances this wake authority with the existing wrapping overflow policy.
    pub(super) fn advance(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

    /// Constructs a generation for an owner-specific overflow test.
    #[cfg(test)]
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }
}

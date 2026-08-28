//! Process-local authority for cached terminal history layouts.

/// Identifies the terminal history content represented by a cached layout.
///
/// This authority starts at zero and wraps when a history mutation advances it,
/// matching the pre-existing terminal layout behavior. It is never serialized.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct TerminalHistoryGeneration(
    /// Process-local scalar retained by the terminal history owner.
    u64,
);

impl TerminalHistoryGeneration {
    /// Creates a history authority from the owning layout state's raw value.
    #[cfg(test)]
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Advances history authority with the existing wrapping overflow policy.
    pub(super) fn advance(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }
}

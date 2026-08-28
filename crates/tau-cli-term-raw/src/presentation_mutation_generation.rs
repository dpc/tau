//! Process-local authority for selected-presentation mutation observations.

/// Identifies the selected-presentation state represented by an observation.
///
/// This authority starts at zero and wraps when a selected-presentation
/// mutation registers, matching the pre-existing observation behavior. It is
/// never serialized.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct PresentationMutationGeneration(
    /// Process-local scalar retained by the presentation observation owner.
    u64,
);

impl PresentationMutationGeneration {
    /// Creates a presentation authority from the owning observation state's raw
    /// value.
    #[cfg(test)]
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Advances presentation authority with the existing wrapping overflow
    /// policy.
    pub(super) fn advance(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

    /// Returns the scalar for a diagnostic boundary.
    pub(super) const fn get(self) -> u64 {
        self.0
    }
}

//! In-process directory-lock wake authority.

/// Generation paired with the directory-lock condition variable.
///
/// Filesystem-backed waiters retain an observed generation across a registry
/// check and wait, so an in-process notification cannot be lost before the
/// counter's deliberately saturating maximum.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct DirLockWakeGeneration(
    /// Process-local scalar retained by directory-lock state.
    u64,
);

impl DirLockWakeGeneration {
    /// Advance this wake authority with the existing saturating overflow
    /// policy.
    #[must_use]
    pub(super) const fn saturating_next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Construct a wake generation for a test-owned directory-lock fixture.
    #[cfg(test)]
    #[must_use]
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }
}

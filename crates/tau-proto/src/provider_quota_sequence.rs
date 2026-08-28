use serde::{Deserialize, Serialize};

/// An epoch-local ordering cursor for provider quota reports and snapshots.
///
/// Zero remains a valid wire value. Advancing the maximum value saturates so
/// provider producers preserve the protocol's existing exhaustion behavior.
#[repr(transparent)]
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct ProviderQuotaSequence(
    /// The scalar epoch-local ordering cursor.
    u64,
);

impl ProviderQuotaSequence {
    /// Constructs a provider quota sequence from its scalar wire value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the scalar wire value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advances this sequence while saturating at the maximum wire value.
    #[must_use]
    pub const fn saturating_next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[cfg(test)]
#[path = "provider_quota_sequence/tests.rs"]
mod tests;

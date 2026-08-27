use serde::{Deserialize, Serialize};

/// A count measured in bytes.
///
/// This type deliberately supports only byte-domain arithmetic. Callers must
/// construct or extract the scalar explicitly at configuration and wire
/// boundaries.
#[repr(transparent)]
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct ByteCount(u64);

impl ByteCount {
    /// Zero bytes.
    pub const ZERO: Self = Self(0);
    /// Largest representable byte count.
    pub const MAX: Self = Self(u64::MAX);

    /// Construct a byte count from a boundary scalar.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the scalar byte count for a boundary API.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Add two byte counts, returning `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Subtract two byte counts, returning `None` on underflow.
    #[must_use]
    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Add two byte counts, saturating at the numeric maximum.
    #[must_use]
    pub const fn saturating_add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }

    /// Subtract two byte counts, saturating at zero.
    #[must_use]
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl std::fmt::Display for ByteCount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

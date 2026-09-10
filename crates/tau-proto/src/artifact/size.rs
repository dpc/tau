use serde::{Deserialize, Serialize};

/// Validated original byte count, independent of decoded media allocations.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct ArtifactSize(
    /// Original length at or below the supported object bound.
    u64,
);

impl ArtifactSize {
    /// Validates the supported original object bound.
    pub fn new(size: u64) -> Result<Self, super::ArtifactError> {
        if super::ARTIFACT_MAX_BYTES < size {
            return Err(super::ArtifactError::Invalid);
        }
        Ok(Self(size))
    }

    /// Returns the validated raw byte count for filesystem/transport APIs.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for ArtifactSize {
    type Error = super::ArtifactError;
    fn try_from(size: u64) -> Result<Self, Self::Error> {
        Self::new(size)
    }
}

impl From<ArtifactSize> for u64 {
    fn from(size: ArtifactSize) -> Self {
        size.get()
    }
}

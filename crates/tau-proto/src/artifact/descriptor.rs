use serde::{Deserialize, Serialize};

use super::{ArtifactError, ArtifactKey, ArtifactSize};

/// Canonical original identity and validated byte length; hints are per-use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    /// Validated original content digest.
    pub key: ArtifactKey,
    /// Validated original byte length.
    pub size: ArtifactSize,
}

impl ArtifactDescriptor {
    /// Validates an original byte length while preserving the typed digest.
    pub fn new(key: ArtifactKey, size: u64) -> Result<Self, ArtifactError> {
        Ok(Self {
            key,
            size: ArtifactSize::new(size)?,
        })
    }
}

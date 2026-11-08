use serde::{Deserialize, Serialize};

use super::{ArtifactError, ArtifactIdentity, ArtifactKey, ArtifactSize};

/// Canonical identity, immutable metadata, and validated payload byte length.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    /// Validated original content digest.
    pub key: ArtifactKey,
    /// Validated original byte length.
    pub size: ArtifactSize,
    /// Missing historical fields retain payload-only verification.
    #[serde(default)]
    pub identity: ArtifactIdentity,
}

impl ArtifactDescriptor {
    /// Validates an original byte length while preserving the typed digest.
    pub fn new(key: ArtifactKey, size: u64) -> Result<Self, ArtifactError> {
        Ok(Self {
            key,
            size: ArtifactSize::new(size)?,
            identity: ArtifactIdentity::LegacyPayload,
        })
    }

    /// Builds a descriptor from complete bytes and a validated identity.
    pub fn from_bytes(bytes: &[u8], identity: ArtifactIdentity) -> Result<Self, ArtifactError> {
        if !identity.is_bounded() {
            return Err(ArtifactError::Invalid);
        }
        Ok(Self {
            key: ArtifactKey::from_digest(identity.digest(bytes).to_hex().as_str())?,
            size: ArtifactSize::new(bytes.len() as u64)?,
            identity,
        })
    }

    /// Verifies payload size and the complete metadata-bound identity.
    pub fn verify(&self, bytes: &[u8]) -> Result<(), ArtifactError> {
        if !self.identity.is_bounded()
            || self.size.get() != bytes.len() as u64
            || !self
                .key
                .matches_digest(self.identity.digest(bytes).to_hex().as_str())
        {
            return Err(ArtifactError::Integrity);
        }
        Ok(())
    }
}

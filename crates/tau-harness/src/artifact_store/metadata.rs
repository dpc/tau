use serde::{Deserialize, Serialize};

/// Canonical age metadata is replaced atomically; original bytes never change.
#[derive(Serialize, Deserialize)]
pub(super) struct Metadata {
    /// Closed schema version; unknown versions are preserved, not guessed.
    pub(super) version: u32,
    /// Exact original byte length.
    pub(super) size: u64,
    /// Last successful new explicit upload, monotonic across clock rollback.
    pub(super) last_put_at: u64,
    /// Historical metadata defaults to payload-only identity.
    #[serde(default)]
    pub(super) identity: tau_proto::ArtifactIdentity,
}

impl Metadata {
    /// Checks the closed storage schema and its corresponding identity scheme.
    pub(super) fn valid(&self) -> bool {
        matches!(
            (&self.identity, self.version),
            (tau_proto::ArtifactIdentity::LegacyPayload, 1)
                | (tau_proto::ArtifactIdentity::MetadataV1 { .. }, 2)
        ) && self.identity.is_bounded()
            && self.size <= tau_proto::ARTIFACT_MAX_BYTES
    }

    /// Reconstructs the exact descriptor without discarding immutable metadata.
    pub(super) fn descriptor(
        &self,
        key: tau_proto::ArtifactKey,
    ) -> Result<tau_proto::ArtifactDescriptor, tau_proto::ArtifactError> {
        if !self.valid() {
            return Err(tau_proto::ArtifactError::Integrity);
        }
        let mut descriptor = tau_proto::ArtifactDescriptor::new(key, self.size)?;
        descriptor.identity = self.identity.clone();
        Ok(descriptor)
    }

    /// Preserves legacy receipt recovery while all new writes use version two.
    pub(super) fn version(identity: &tau_proto::ArtifactIdentity) -> u32 {
        match identity {
            tau_proto::ArtifactIdentity::LegacyPayload => 1,
            tau_proto::ArtifactIdentity::MetadataV1 { .. } => 2,
        }
    }
}

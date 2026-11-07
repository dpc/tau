use super::identifier::artifact_id;

artifact_id!(
    ArtifactKey,
    "Fixed 96-bit BLAKE3 identity, or a deprecated full digest retained for verification.",
    valid_key
);

impl ArtifactKey {
    /// Constructs the canonical identity from a full lowercase BLAKE3 digest.
    pub fn from_digest(hex: &str) -> Result<Self, super::ArtifactError> {
        if super::artifact_digest(&format!("blake3:{hex}")).is_none() {
            return Err(super::ArtifactError::Invalid);
        }
        Self::parse(&hex[..super::ARTIFACT_DIGEST_HEX_LEN])
    }

    /// Returns the canonical fixed-width identity, including for legacy keys.
    #[must_use]
    pub fn canonical(&self) -> &str {
        &super::artifact_digest(self).expect("validated key")[..super::ARTIFACT_DIGEST_HEX_LEN]
    }

    /// Checks the full digest for legacy keys and exactly 96 bits for new keys.
    #[must_use]
    pub fn matches_digest(&self, hex: &str) -> bool {
        super::artifact_digest(self)
            .is_some_and(|expected| hex.len() == 64 && hex.starts_with(expected))
    }
}

fn valid_key(text: &str) -> bool {
    super::artifact_digest(text).is_some()
}

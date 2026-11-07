use super::identifier::artifact_id;

artifact_id!(
    ArtifactKey,
    "Fixed 96-bit BLAKE3 artifact identity.",
    valid_key
);

impl ArtifactKey {
    /// Constructs the canonical identity from a full lowercase BLAKE3 digest.
    pub fn from_digest(hex: &str) -> Result<Self, super::ArtifactError> {
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(super::ArtifactError::Invalid);
        }
        Self::parse(&hex[..super::ARTIFACT_DIGEST_HEX_LEN])
    }

    /// Returns the canonical fixed-width identity.
    #[must_use]
    pub fn canonical(&self) -> &str {
        self.as_str()
    }

    /// Checks the fixed 96-bit identity against a full digest.
    #[must_use]
    pub fn matches_digest(&self, hex: &str) -> bool {
        super::artifact_digest(self)
            .is_some_and(|expected| hex.len() == 64 && hex.starts_with(expected))
    }
}

fn valid_key(text: &str) -> bool {
    super::artifact_digest(text).is_some()
}

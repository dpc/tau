//! Private physical locators, distinct from public artifact identities.

#[cfg(test)]
mod tests;

use tau_proto::{ArtifactError, ArtifactKey};

/// Validated full BLAKE3 filename in the private CAS catalogs.
///
/// Unlike `ArtifactKey`'s 24-digit public identity and strict wire serde, this
/// 64-digit locator never crosses the storage boundary or enters generic
/// notices. See `SPEC-shared-artifacts` for identity and collision rules.
#[derive(Clone, Eq, PartialEq)]
pub(super) struct StorageDigest(
    /// Exactly 64 lowercase hexadecimal digits.
    String,
);

impl StorageDigest {
    /// Validates a physical catalog filename without accepting public
    /// spellings.
    pub(super) fn parse(hex: &str) -> Result<Self, ArtifactError> {
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ArtifactError::Invalid);
        }
        Ok(Self(hex.to_owned()))
    }

    /// Constructs a locator from a computed metadata-bound or legacy hash.
    pub(super) fn from_hash(hash: blake3::Hash) -> Self {
        Self(hash.to_hex().to_string())
    }

    /// Returns the explicit private filename spelling for filesystem joins.
    pub(super) fn as_hex(&self) -> &str {
        &self.0
    }

    /// Derives the fixed public identity without publishing the full locator.
    pub(super) fn key(&self) -> ArtifactKey {
        ArtifactKey::from_digest(self.as_hex()).expect("validated storage digest")
    }
}

impl std::fmt::Debug for StorageDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StorageDigest(<private>)")
    }
}

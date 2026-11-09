use super::{ArtifactError, ArtifactKey, parse_artifact_reference};

/// Artifact tool/UI input normalized to one canonical identity.
///
/// Prefer a bare 24-digit lowercase hex key. Deserialization also accepts a
/// `tau-artifact:KEY` URI or Markdown `<tau-artifact:KEY>` autolink for
/// convenient copying. Serialization always emits the bare key. Use this type
/// in tool argument structs, and keep strict `ArtifactKey` fields in
/// wire/history DTOs.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "ArtifactKey")]
pub struct ArtifactReference {
    /// Validated identity, independent of the supplied presentation spelling.
    key: ArtifactKey,
}

impl ArtifactReference {
    /// Normalizes a bare key, plain URI, or Markdown autolink.
    pub fn parse(text: &str) -> Result<Self, ArtifactError> {
        parse_artifact_reference(text).map(Self::from)
    }

    /// Borrows the normalized identity for a transfer or explicit formatting.
    #[must_use]
    pub fn key(&self) -> &ArtifactKey {
        &self.key
    }

    /// Consumes the input wrapper to obtain its strict canonical identity.
    #[must_use]
    pub fn into_key(self) -> ArtifactKey {
        self.key
    }
}

impl TryFrom<String> for ArtifactReference {
    type Error = ArtifactError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl std::str::FromStr for ArtifactReference {
    type Err = ArtifactError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl From<ArtifactKey> for ArtifactReference {
    fn from(key: ArtifactKey) -> Self {
        Self { key }
    }
}

impl From<ArtifactReference> for ArtifactKey {
    fn from(reference: ArtifactReference) -> Self {
        reference.into_key()
    }
}

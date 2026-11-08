use serde::{Deserialize, Serialize};

use super::ArtifactError;

#[cfg(test)]
mod tests;

/// Maximum UTF-8 byte length of each immutable metadata field.
pub const ARTIFACT_METADATA_BYTES: usize = 255;

/// Truncates producer metadata at a UTF-8 boundary before binding its identity.
pub fn bounded_artifact_metadata(value: &str) -> String {
    let mut end = value.len().min(ARTIFACT_METADATA_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Hash scheme and immutable metadata belonging to an artifact.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "IdentityWire")]
pub enum ArtifactIdentity {
    /// Historical objects hash only the payload and carry no metadata.
    #[default]
    LegacyPayload,
    /// Version-one domain-separated metadata and payload identity.
    MetadataV1 {
        /// Optional original basename, not filesystem path authority.
        filename: Option<String>,
        /// Optional declared media type, not content validation.
        mime_type: Option<String>,
    },
}

/// Deserialization shape validated before metadata enters protocol or storage.
#[derive(Deserialize)]
enum IdentityWire {
    /// Historical payload-only hash.
    LegacyPayload,
    /// Version-one metadata fields.
    MetadataV1 {
        /// Optional original basename.
        filename: Option<String>,
        /// Optional media declaration.
        mime_type: Option<String>,
    },
}

impl TryFrom<IdentityWire> for ArtifactIdentity {
    type Error = &'static str;

    fn try_from(value: IdentityWire) -> Result<Self, Self::Error> {
        match value {
            IdentityWire::LegacyPayload => Ok(Self::LegacyPayload),
            IdentityWire::MetadataV1 {
                filename,
                mime_type,
            } => Self::metadata(filename, mime_type).map_err(|_| "artifact metadata exceeds bound"),
        }
    }
}

impl ArtifactIdentity {
    /// Selects the current scheme for a new artifact without metadata.
    pub fn anonymous() -> Self {
        Self::MetadataV1 {
            filename: None,
            mime_type: None,
        }
    }

    /// Constructs bounded immutable metadata for a new artifact.
    pub fn metadata(
        filename: Option<String>,
        mime_type: Option<String>,
    ) -> Result<Self, ArtifactError> {
        let identity = Self::MetadataV1 {
            filename,
            mime_type,
        };
        if !identity.is_bounded() {
            return Err(ArtifactError::Invalid);
        }
        Ok(identity)
    }

    /// Checks metadata bounds even for values constructed directly in Rust.
    pub fn is_bounded(&self) -> bool {
        match self {
            Self::LegacyPayload => true,
            Self::MetadataV1 {
                filename,
                mime_type,
            } => [filename, mime_type].iter().all(|field| {
                field
                    .as_ref()
                    .is_none_or(|s| s.len() <= ARTIFACT_METADATA_BYTES)
            }),
        }
    }

    /// Returns the recorded basename, without granting path authority.
    pub fn filename(&self) -> Option<&str> {
        match self {
            Self::LegacyPayload => None,
            Self::MetadataV1 { filename, .. } => filename.as_deref(),
        }
    }

    /// Returns the recorded media declaration, without validating content.
    pub fn mime_type(&self) -> Option<&str> {
        match self {
            Self::LegacyPayload => None,
            Self::MetadataV1 { mime_type, .. } => mime_type.as_deref(),
        }
    }

    /// Starts the shared streaming hash with unambiguous, versioned framing.
    pub fn hasher(&self, payload_size: u64) -> blake3::Hasher {
        let mut hasher = blake3::Hasher::new();
        if let Self::MetadataV1 {
            filename,
            mime_type,
        } = self
        {
            hasher.update(b"tau.artifact\0\x01");
            for field in [filename, mime_type] {
                match field {
                    None => {
                        hasher.update(&[0]);
                    }
                    Some(value) => {
                        hasher.update(&[1]);
                        hasher.update(&(value.len() as u64).to_be_bytes());
                        hasher.update(value.as_bytes());
                    }
                }
            }
            hasher.update(&payload_size.to_be_bytes());
        }
        hasher
    }

    /// Hashes raw payload bytes using this identity's shared framing.
    pub fn digest(&self, bytes: &[u8]) -> blake3::Hash {
        let mut hasher = self.hasher(bytes.len() as u64);
        hasher.update(bytes);
        hasher.finalize()
    }
}

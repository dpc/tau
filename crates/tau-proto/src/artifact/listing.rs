use serde::{Deserialize, Serialize};

use super::ArtifactDescriptor;

/// Hard page bound for UI-only artifact discovery.
pub const ARTIFACT_LIST_LIMIT: u8 = 10;

/// Advisory metadata, not a retention pin or verified content description.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactListEntry {
    /// Full insertion key and original byte size.
    pub descriptor: ArtifactDescriptor,
    /// Latest explicit put timestamp; reads do not refresh it.
    pub last_put_at: u64,
    /// Optional header inference, never a content validity claim.
    pub signature: Option<ArtifactSignature>,
}

impl std::fmt::Debug for ArtifactListEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ArtifactListEntry(<private>)")
    }
}

/// Small closed vocabulary of inert header inferences.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSignature {
    /// PNG signature.
    Png,
    /// JPEG start marker.
    Jpeg,
    /// GIF version header.
    Gif,
    /// WebP RIFF container header.
    Webp,
    /// PDF header.
    Pdf,
}

impl ArtifactSignature {
    /// Returns the inferred media declaration, not a validity guarantee.
    #[must_use]
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Pdf => "application/pdf",
        }
    }
}

/// Accepts only prefixes of the canonical bare-key grammar.
#[must_use]
pub fn artifact_prefix_valid(prefix: &str) -> bool {
    prefix.len() <= super::ARTIFACT_DIGEST_HEX_LEN
        && prefix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

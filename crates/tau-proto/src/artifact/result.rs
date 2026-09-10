use serde::{Deserialize, Serialize};

use super::{
    ArtifactDescriptor, ArtifactError, ArtifactReadId, ArtifactRequestId, ArtifactUploadId,
};

/// One directed correlated transfer response.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactResult {
    /// Exact request correlation.
    pub request_id: ArtifactRequestId,
    /// Success value or fixed error classification.
    pub result: Result<ArtifactValue, ArtifactError>,
}

/// Successful transfer values.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "value", rename_all = "snake_case")]
pub enum ArtifactValue {
    /// Availability, cancellation, or close acknowledged.
    Done,
    /// A newly admitted upload.
    Upload {
        /// Transient upload identity.
        upload: ArtifactUploadId,
    },
    /// Next accepted write offset.
    Written {
        /// End of all currently accepted bytes.
        next_offset: u64,
    },
    /// Finalized or inspected original identity.
    Descriptor(ArtifactDescriptor),
    /// Opened original with a bounded read lifetime.
    Opened {
        /// Transient read identity.
        read: ArtifactReadId,
        /// Immutable identity to verify after transfer.
        descriptor: ArtifactDescriptor,
    },
    /// Original byte range from an open transfer.
    Chunk {
        /// Requested absolute offset.
        offset: u64,
        /// Original bytes; omitted from Debug.
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
        /// Whether this range reaches the original end.
        eof: bool,
    },
}

impl std::fmt::Debug for ArtifactValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ArtifactValue(<private>)")
    }
}
impl std::fmt::Debug for ArtifactResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ArtifactResult(<private>)")
    }
}

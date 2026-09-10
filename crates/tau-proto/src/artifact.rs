//! Directed immutable artifact transfers, independent of journals and extension
//! data.

use serde::{Deserialize, Serialize};

mod descriptor;
mod error;
mod frame;
mod identifier;
mod key;
mod read_id;
mod request_id;
mod result;
mod size;
#[cfg(test)]
mod tests;
mod upload_id;
pub use descriptor::ArtifactDescriptor;
pub use error::ArtifactError;
pub use frame::artifact_frame_fits;
pub use key::ArtifactKey;
pub use read_id::ArtifactReadId;
pub use request_id::ArtifactRequestId;
pub use result::{ArtifactResult, ArtifactValue};
pub use size::ArtifactSize;
pub use upload_id::ArtifactUploadId;

/// Maximum original object size, independent of image preview limits.
pub const ARTIFACT_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum raw payload in either transfer direction.
pub const ARTIFACT_CHUNK_BYTES: usize = 1024 * 1024;
/// Maximum complete encoded Artifact request or response frame.
pub const ARTIFACT_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// One correlated, exact-session request from a configured extension.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRequest {
    /// Caller-selected correlation, bounded to 128 ASCII identifier bytes.
    pub request_id: ArtifactRequestId,
    /// Session binding that must still be current at admission.
    pub expected_session_id: crate::SessionId,
    /// Transient transfer operation; never a journal event.
    pub op: ArtifactOp,
}

impl ArtifactRequest {
    /// Validates raw bounds before cloning, queueing, or filesystem access.
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        if !artifact_identifier(&self.request_id) {
            return false;
        }
        match &self.op {
            ArtifactOp::Available => true,
            ArtifactOp::Begin { .. } => true,
            ArtifactOp::Write {
                upload,
                offset,
                bytes,
            } => {
                artifact_identifier(upload)
                    && bytes.len() <= ARTIFACT_CHUNK_BYTES
                    && offset
                        .checked_add(bytes.len() as u64)
                        .is_some_and(|end| end <= ARTIFACT_MAX_BYTES)
            }
            ArtifactOp::Finalize { upload } | ArtifactOp::Abort { upload } => {
                artifact_identifier(upload)
            }
            ArtifactOp::Stat { key } | ArtifactOp::Open { key } => artifact_digest(key).is_some(),
            ArtifactOp::Read {
                read,
                offset,
                length,
            } => {
                artifact_identifier(read)
                    && *offset <= ARTIFACT_MAX_BYTES
                    && *length > 0
                    && *length as usize <= ARTIFACT_CHUNK_BYTES
            }
            ArtifactOp::Close { read } => artifact_identifier(read),
        }
    }
}

/// Bounded artifact operations; keys grant no list, mutation, or deletion
/// operation.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ArtifactOp {
    /// Check persistent store availability before an expensive producer effect.
    Available,
    /// Allocate a connection-owned upload; a new upload renews age on commit.
    Begin {
        /// Exact expected original size.
        size: ArtifactSize,
    },
    /// Append the next chunk or retransmit identical already accepted bytes.
    Write {
        /// Transient upload identity, not a public read key.
        upload: ArtifactUploadId,
        /// Absolute byte offset in the original.
        offset: u64,
        /// Original bytes, never included in Debug projections.
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    /// Publish complete synced bytes; recognized retries do not renew age.
    Finalize {
        /// Upload identity; completed receipts survive reconnection and
        /// restart.
        upload: ArtifactUploadId,
    },
    /// Cancel unfinished staging without deleting any committed shared object.
    Abort {
        /// Upload identity owned by this configured instance and session.
        upload: ArtifactUploadId,
    },
    /// Inspect immutable metadata without renewing age or pinning retention.
    Stat {
        /// Direct content digest key.
        key: ArtifactKey,
    },
    /// Open a bounded read transfer; cleanup skips active transfers.
    Open {
        /// Direct content digest key.
        key: ArtifactKey,
    },
    /// Read a bounded chunk from an open transfer without renewing age.
    Read {
        /// Connection-owned read identity.
        read: ArtifactReadId,
        /// Absolute original byte offset.
        offset: u64,
        /// Requested raw byte count, at most `ARTIFACT_CHUNK_BYTES`.
        length: u32,
    },
    /// Release an open transfer early; natural expiry also releases it.
    Close {
        /// Connection-owned read identity.
        read: ArtifactReadId,
    },
}

// Deliberately opaque: neither original bytes nor lookup/transfer keys belong
// in logs.
impl std::fmt::Debug for ArtifactOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ArtifactOp(<private>)")
    }
}
impl std::fmt::Debug for ArtifactRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ArtifactRequest(<private>)")
    }
}

/// Checks the exact canonical digest spelling without accessing storage.
#[must_use]
pub fn artifact_digest(key: &str) -> Option<&str> {
    let hex = key.strip_prefix("blake3:")?;
    (hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(hex)
}

/// Checks bounded RPC and operation identifiers before path construction.
#[must_use]
pub fn artifact_identifier(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

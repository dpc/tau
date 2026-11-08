use std::time::Instant;

use tau_proto::ArtifactSize;

/// Incomplete bytes have no durable identity until finalization starts.
pub(super) struct Upload {
    /// Stable configured instance and exact session for durable retry
    /// authority.
    pub(super) owner: String,
    /// Connection generation permitted to continue this unfinished operation.
    pub(super) connection: String,
    /// Exact expected original length.
    pub(super) size: ArtifactSize,
    /// Immutable hash scheme and metadata admitted at Begin.
    pub(super) identity: tau_proto::ArtifactIdentity,
    /// Already accepted original bytes.
    pub(super) bytes: Vec<u8>,
    /// Non-renewable expiry.
    pub(super) expires: Instant,
}

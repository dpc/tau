use serde::{Deserialize, Serialize};
use tau_proto::ArtifactDescriptor;

/// Durable finalization intent and receipt; a retry uses this fixed timestamp.
#[derive(Serialize, Deserialize)]
pub(super) struct Receipt {
    /// Closed schema version.
    pub(super) version: u32,
    /// Stable instance/session owner, never an authority supplied by the peer.
    pub(super) owner: String,
    /// Original digest and length.
    pub(super) descriptor: ArtifactDescriptor,
    /// Time fixed before first publication attempt; retries never refresh it.
    pub(super) put_at: u64,
    /// Durable receipt expiry, independent of original retention.
    pub(super) expires_at: u64,
    /// True only after original publication and its sync boundary completed.
    pub(super) complete: bool,
}

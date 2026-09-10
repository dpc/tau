use serde::{Deserialize, Serialize};

/// Canonical age metadata is replaced atomically; original bytes never change.
#[derive(Serialize, Deserialize)]
pub(super) struct Metadata {
    /// Closed schema version; unknown versions are preserved, not guessed.
    pub(super) version: u32,
    /// Exact original byte length.
    pub(super) size: u64,
    /// Last successful new explicit upload, monotonic across clock rollback.
    pub(super) last_put_at: u64,
}

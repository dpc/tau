use serde::{Deserialize, Serialize};

/// Fixed, payload-free failures; unknown outcomes may already have committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactError {
    /// Persistent artifacts are disabled, or the peer has no RPC authority.
    Permission,
    /// The request targets a different session.
    SessionMismatch,
    /// Invalid identifier, offset, size, frame, or operation shape.
    Invalid,
    /// Transfer capacity or cross-process coordination is currently busy.
    Busy,
    /// Object or bounded transfer/receipt no longer exists.
    Unavailable,
    /// Original content or canonical metadata failed validation.
    Integrity,
    /// Filesystem operation failed; publication outcome may be unknown.
    Io,
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "artifact operation failed: {self:?}")
    }
}

impl std::error::Error for ArtifactError {}

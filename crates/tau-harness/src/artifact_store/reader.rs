use std::fs::File;
use std::time::Instant;

/// An active bounded read pins cleanup, not journal references or key sharing.
pub(super) struct Reader {
    /// Connection generation authorized to use the read identity.
    pub(super) connection: String,
    /// Read-only original byte handle.
    pub(super) data: File,
    /// Stable shared lock, independent of detachable object directories.
    pub(super) _lock: File,
    /// Original byte length.
    pub(super) size: u64,
    /// Non-renewable expiry.
    pub(super) expires: Instant,
}

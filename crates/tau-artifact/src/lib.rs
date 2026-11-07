//! Versioned directory bundles. Identity covers the marker and compressed
//! bytes; MIME is only a hint. This codec never invokes external archive
//! programs.

use std::io;

mod bounded_io;
mod decode;
mod encode;
#[cfg(test)]
mod tests;

pub use decode::unpack;
pub use encode::pack;

/// Advisory media hint for the versioned Tau directory format.
pub const DIRECTORY_MIME: &str = "application/vnd.tau.directory";
/// Reserved byte prefix identifying directory bundles, including unknown
/// versions.
const MAGIC: &[u8] = b"TAU-DIRECTORY\0";
/// Version one contains exactly one zstd frame holding a restricted tar stream.
const VERSION: u8 = 1;
/// Maximum number of files and directories, including empty directories.
const MAX_ENTRIES: usize = 10_000;
/// Maximum UTF-8 archive path length supported by the ustar subset.
const MAX_PATH_BYTES: usize = 255;
/// Maximum nesting bounds stack use and filesystem operations.
const MAX_DEPTH: usize = 32;

/// Operational byte limits supplied by the caller, independent of transport.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Complete encoded bytes, including the identifying header.
    pub stored_bytes: u64,
    /// Aggregate logical regular-file bytes, not compressed sizes.
    pub expanded_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            stored_bytes: 32 * 1024 * 1024,
            expanded_bytes: 32 * 1024 * 1024,
        }
    }
}

impl Limits {
    /// Bounds the complete decoded tar, including headers, padding and end
    /// blocks.
    fn stream_bytes(self) -> u64 {
        self.expanded_bytes.saturating_add(16 * 1024 * 1024)
    }
}

/// Identifies reserved directory bytes without trusting filename or MIME hints.
pub fn is_directory(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn check_cancel(cancelled: &impl Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        // `Read` callers retry Interrupted automatically; cancellation is
        // final.
        Err(io::Error::other("artifact cancelled"))
    } else {
        Ok(())
    }
}

/// Accept only unambiguous portable relative UTF-8 names, never sanitize paths.
fn check_path(path: &str) -> io::Result<()> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.contains(['\\', ':', '\0'])
        || path.split('/').count() > MAX_DEPTH
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(invalid("unsupported directory member path"));
    }
    Ok(())
}

//! Foreground launch identity handoff, separate from protocol and diagnostic
//! I/O.

use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;
use tau_proto::SessionId;

/// Maximum encoded record size, including its terminating newline.
const MAX_RECORD_BYTES: usize = 64 * 1024;

/// Exact startup identity, encoded before startup can emit any stdout bytes.
#[derive(Serialize)]
struct ReadyIdentity<'a> {
    /// Validated session owned by this foreground daemon.
    session_id: &'a SessionId,
    /// Canonical startup directory, never a lossy path rendering.
    project_root: &'a Path,
}

/// Prevalidated single-line stdout handoff for a successfully started daemon.
pub(super) struct ReadyRecord {
    /// Complete JSON and newline, bounded before publication.
    bytes: Vec<u8>,
}

impl ReadyRecord {
    /// Encode exact identity or fail without publishing a partial record.
    pub(super) fn encode(session_id: &SessionId, project_root: &Path) -> io::Result<Self> {
        let mut bytes = serde_json::to_vec(&ReadyIdentity {
            session_id,
            project_root,
        })?;
        bytes.push(b'\n');
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "serve readiness JSON exceeds 64 KiB",
            ));
        }
        Ok(Self { bytes })
    }

    /// Publish and flush once; any I/O failure aborts foreground startup.
    pub(super) fn write_to(self, mut writer: impl Write) -> io::Result<()> {
        writer.write_all(&self.bytes)?;
        writer.flush()
    }
}

#[cfg(test)]
mod tests;

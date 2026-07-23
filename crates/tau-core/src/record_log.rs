//! Shared helpers for length-prefixed durable record logs.

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Largest individual CBOR record that durable journal readers allocate.
///
/// A torn or corrupt length header can otherwise request an effectively
/// unbounded allocation. Writers use the same limit so every committed record
/// remains readable by the matching loader.
pub(crate) const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// Byte offsets for one successfully committed frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FrameAppend {
    /// Exact EOF captured before writing the length prefix.
    pub start_offset: u64,
    /// Exact EOF after the complete length prefix and payload.
    pub end_offset: u64,
}

/// One failed frame append and the status of its durable rollback.
#[derive(Debug)]
struct FrameAppendError {
    /// Original prefix, payload, or commit-sync failure.
    source: io::Error,
    /// Rollback failure, when the stream can no longer be trusted for appends.
    rollback: Option<io::Error>,
}

/// Minimal operations needed to append and roll back one framed record.
trait FrameIo {
    /// Returns the exact current EOF and positions writes there.
    fn seek_to_end(&mut self) -> io::Result<u64>;

    /// Writes every byte or returns the first write failure.
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Truncates the stream to the supplied byte offset.
    fn truncate(&mut self, offset: u64) -> io::Result<()>;

    /// Makes preceding data or truncation changes durable.
    fn sync_data(&mut self) -> io::Result<()>;
}

impl FrameIo for File {
    fn seek_to_end(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::End(0))
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        Write::write_all(self, bytes)
    }

    fn truncate(&mut self, offset: u64) -> io::Result<()> {
        self.set_len(offset)
    }

    fn sync_data(&mut self) -> io::Result<()> {
        File::sync_data(self)
    }
}

/// Per-process append safety state shared by the durable store writers.
///
/// A path enters `poisoned` only when Tau cannot durably restore its exact
/// pre-append EOF. Later appends reject that path before opening it.
#[derive(Debug, Default)]
pub(crate) struct FramedAppendState {
    /// Journal paths whose last rollback could not be made durable.
    poisoned: HashSet<PathBuf>,
    /// Deterministic one-shot append faults used by store-level tests.
    #[cfg(test)]
    faults: std::collections::HashMap<PathBuf, AppendFault>,
}

impl FramedAppendState {
    /// Rejects a poisoned journal before its caller opens or otherwise mutates
    /// it.
    pub(crate) fn ensure_appendable(&self, path: &Path) -> io::Result<()> {
        if self.poisoned.contains(path) {
            Err(io::Error::other(
                "journal append disabled after an incomplete durable rollback",
            ))
        } else {
            Ok(())
        }
    }

    /// Appends one complete frame and records rollback uncertainty.
    pub(crate) fn append(
        &mut self,
        path: &Path,
        file: &mut File,
        payload: &[u8],
    ) -> io::Result<FrameAppend> {
        self.ensure_appendable(path)?;
        #[cfg(test)]
        let result = if let Some(fault) = self.faults.remove(path) {
            append_frame(
                &mut FaultInjectingFile::new(file, fault, 8_usize.saturating_add(payload.len())),
                payload,
            )
        } else {
            append_frame(file, payload)
        };
        #[cfg(not(test))]
        let result = append_frame(file, payload);

        match result {
            Ok(appended) => Ok(appended),
            Err(error) => {
                if error.rollback.is_some() {
                    self.poisoned.insert(path.to_path_buf());
                }
                Err(error.source)
            }
        }
    }

    /// Installs one deterministic fault for the next append to `path`.
    #[cfg(test)]
    pub(crate) fn inject_fault(&mut self, path: impl Into<PathBuf>, fault: AppendFault) {
        self.faults.insert(path.into(), fault);
    }
}

/// One deterministic failure plan for a test append.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AppendFault {
    /// Frame-byte offset that fails before that byte is written.
    pub fail_write_at: Option<usize>,
    /// Whether the frame commit sync fails.
    pub fail_commit_sync: bool,
    /// Whether rollback truncation fails.
    pub fail_truncate: bool,
    /// Whether the sync that should make rollback durable fails.
    pub fail_rollback_sync: bool,
}

/// A real file wrapped with one deterministic append failure plan.
#[cfg(test)]
struct FaultInjectingFile<'a> {
    /// Real journal mutated by operations that the plan allows.
    file: &'a mut File,
    /// Faults selected for this append.
    fault: AppendFault,
    /// Number of frame bytes successfully written.
    written: usize,
    /// Complete prefix-plus-payload byte length.
    frame_bytes: usize,
    /// Number of data-sync calls made by the append primitive.
    sync_calls: usize,
}

#[cfg(test)]
impl<'a> FaultInjectingFile<'a> {
    /// Wraps `file` with `fault`.
    fn new(file: &'a mut File, fault: AppendFault, frame_bytes: usize) -> Self {
        Self {
            file,
            fault,
            written: 0,
            frame_bytes,
            sync_calls: 0,
        }
    }
}

#[cfg(test)]
impl FrameIo for FaultInjectingFile<'_> {
    fn seek_to_end(&mut self) -> io::Result<u64> {
        self.file.seek(SeekFrom::End(0))
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(fail_at) = self.fault.fail_write_at {
            if fail_at <= self.written {
                return Err(injected_error("frame write"));
            }
            if fail_at < self.written.saturating_add(bytes.len()) {
                let accepted = fail_at - self.written;
                Write::write_all(self.file, &bytes[..accepted])?;
                self.written += accepted;
                return Err(injected_error("frame write"));
            }
        }
        Write::write_all(self.file, bytes)?;
        self.written = self.written.saturating_add(bytes.len());
        Ok(())
    }

    fn truncate(&mut self, offset: u64) -> io::Result<()> {
        if self.fault.fail_truncate {
            Err(injected_error("rollback truncate"))
        } else {
            self.file.set_len(offset)
        }
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.sync_calls = self.sync_calls.saturating_add(1);
        let is_commit_sync = self.sync_calls == 1 && self.written == self.frame_bytes;
        if (is_commit_sync && self.fault.fail_commit_sync)
            || (!is_commit_sync && self.fault.fail_rollback_sync)
        {
            Err(injected_error("data sync"))
        } else {
            self.file.sync_data()
        }
    }
}

/// Creates a stable injected I/O error for assertions.
#[cfg(test)]
fn injected_error(operation: &str) -> io::Error {
    io::Error::other(format!("injected {operation} failure"))
}

/// Writes and durably commits one `[u64 length][payload]` frame.
fn append_frame(io: &mut impl FrameIo, payload: &[u8]) -> Result<FrameAppend, FrameAppendError> {
    let start_offset = io.seek_to_end().map_err(|source| FrameAppendError {
        source,
        rollback: None,
    })?;
    let length = u64::try_from(payload.len())
        .expect("usize payload length always fits the journal's u64 frame prefix");
    let append_result = io
        .write_all(&length.to_le_bytes())
        .and_then(|()| io.write_all(payload))
        .and_then(|()| io.sync_data());
    if let Err(source) = append_result {
        let truncate_error = io.truncate(start_offset).err();
        let rollback_sync_error = io.sync_data().err();
        return Err(FrameAppendError {
            source,
            rollback: truncate_error.or(rollback_sync_error),
        });
    }
    Ok(FrameAppend {
        start_offset,
        end_offset: start_offset.saturating_add(8).saturating_add(length),
    })
}

/// Reads the next little-endian record length from a durable record log.
///
/// Clean EOF before a new 8-byte length header means the log ended normally and
/// returns `Ok(None)`. EOF after only part of the header is a torn write and
/// returns `UnexpectedEof` so replay fails closed instead of silently
/// truncating durable state.
pub(crate) fn read_record_length(reader: &mut impl Read) -> io::Result<Option<u64>> {
    let mut length_bytes = [0_u8; 8];
    let bytes_read = match reader.read(&mut length_bytes)? {
        0 => return Ok(None),
        bytes_read => bytes_read,
    };
    if bytes_read < length_bytes.len() {
        reader.read_exact(&mut length_bytes[bytes_read..])?;
    }
    Ok(Some(u64::from_le_bytes(length_bytes)))
}

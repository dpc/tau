//! Shared helpers for length-prefixed durable record logs.

use std::fs as path_std_fs;

#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

#[cfg(test)]
use crate::journal_sync::BlockingSyncHandle;
#[cfg(test)]
use crate::journal_sync::DirtyTargetSnapshot;
use crate::journal_sync::JournalSyncWorker;

/// Largest individual CBOR record that durable journal readers allocate.
///
/// A torn or corrupt length header can otherwise request an effectively
/// unbounded allocation. Writers use the same limit so every committed record
/// remains readable by the matching loader.
pub(crate) const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// Returns the currently missing path components that `create_dir_all(path)`
/// will create, from the highest missing ancestor through `path`.
pub(crate) fn missing_directories(path: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut candidate = path;
    while !candidate.exists() {
        missing.push(candidate.to_path_buf());
        let Some(parent) = candidate.parent() else {
            break;
        };
        if parent == candidate {
            break;
        }
        candidate = parent;
    }
    missing.reverse();
    missing
}

/// Byte offsets for one successfully committed frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FrameAppend {
    /// Exact EOF captured before writing the length prefix.
    pub start_offset: u64,
    /// Exact EOF after the complete length prefix and payload.
    pub end_offset: u64,
}

/// Records retained by recovery and whether it truncated an incomplete EOF
/// tail.
pub(crate) struct RecoveredRecords<T> {
    /// Fully decoded and caller-validated records; complete invalid frames
    /// instead return an error before this value is produced.
    pub records: Vec<T>,
    /// Whether recovery removed an incomplete EOF crash tail.
    pub repaired: bool,
}

/// One failed frame append and the status of its exact-EOF rollback.
#[derive(Debug)]
struct FrameAppendError {
    /// Original prefix or payload-write failure.
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
}

/// Per-process append safety state shared by the durable store writers.
///
/// A path enters `poisoned` only when Tau cannot restore its exact pre-append
/// EOF. Later appends reject that path before opening it.
#[derive(Debug, Default)]
pub(crate) struct FramedAppendState {
    /// Journal paths whose last rollback could not restore the old EOF.
    poisoned: std::collections::HashSet<PathBuf>,
    /// Coalesced nonblocking background writeback.
    sync_worker: JournalSyncWorker,
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
                "journal append disabled after an incomplete rollback",
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
            append_frame(&mut FaultInjectingFile::new(file, fault), payload)
        } else {
            append_frame(file, payload)
        };
        #[cfg(not(test))]
        let result = append_frame(file, payload);

        match result {
            Ok(appended) => {
                self.sync_worker
                    .mark_dirty(path, appended.end_offset, std::iter::empty());
                Ok(appended)
            }
            Err(error) => {
                if error.rollback.is_some() {
                    self.poisoned.insert(path.to_path_buf());
                }
                Err(error.source)
            }
        }
    }

    /// Queues one deepest-boundary target from an ancestor-to-descendant chain
    /// created by the store.
    pub(crate) fn note_created_directories(
        &mut self,
        directories: impl IntoIterator<Item = PathBuf>,
    ) {
        let directories: Vec<_> = directories.into_iter().collect();
        let Some(deepest) = directories.last() else {
            return;
        };
        let parents = directories
            .iter()
            .filter_map(|directory| directory.parent().map(normalized_directory));
        self.sync_worker.mark_directory_boundary(deepest, parents);
    }

    /// Re-covers one store boundary so a prior process cannot strand its entry.
    pub(crate) fn note_directory_boundary(&mut self, directory: &Path) {
        let parent = directory
            .parent()
            .map(normalized_directory)
            .unwrap_or_else(|| PathBuf::from("."));
        self.sync_worker
            .mark_directory_boundary(directory, [parent]);
    }

    /// Uses `directory` as the primary boundary and covers every normalized
    /// ancestor through `.` for relative paths or `/` for absolute paths.
    pub(crate) fn note_directory_boundary_chain(&mut self, directory: &Path) {
        self.sync_worker
            .mark_directory_boundary(directory, directory_ancestors(directory));
    }

    /// Queues a newly created journal and its immediate parent-entry coverage.
    pub(crate) fn note_created_journal(&mut self, path: &Path, file: &File) {
        let directories = path.parent().map(normalized_directory);
        let end_offset = file.metadata().map_or(0, |metadata| metadata.len());
        self.sync_worker.mark_dirty(path, end_offset, directories);
    }

    /// Loads the decoded and caller-validated stream, truncating only an
    /// incomplete frame header or payload at EOF.
    pub(crate) fn recover<T, F>(
        &mut self,
        path: &Path,
        validate: F,
    ) -> io::Result<RecoveredRecords<T>>
    where
        T: DeserializeOwned,
        F: FnMut(&T) -> bool,
    {
        if !path.exists() {
            return Ok(RecoveredRecords {
                records: Vec::new(),
                repaired: false,
            });
        }
        let recovery = recover_prefix(path, validate)?;
        let repaired = recovery.truncated_to.is_some();
        if let Some(end_offset) = recovery.truncated_to {
            self.sync_worker
                .mark_dirty(path, end_offset, std::iter::empty());
        }
        Ok(RecoveredRecords {
            records: recovery.records,
            repaired,
        })
    }

    /// Installs one deterministic fault for the next append to `path`.
    #[cfg(test)]
    pub(crate) fn inject_fault(&mut self, path: impl Into<PathBuf>, fault: AppendFault) {
        self.faults.insert(path.into(), fault);
    }

    /// Prevents worker startup so tests can inspect retained dirty targets.
    #[cfg(test)]
    pub(crate) fn inject_sync_spawn_failure(&self) {
        self.sync_worker.inject_spawn_failure();
    }

    /// Installs a deterministic backend that blocks journal file sync.
    #[cfg(test)]
    pub(crate) fn inject_blocking_sync(&mut self) -> BlockingSyncHandle {
        let (worker, handle) = JournalSyncWorker::blocking_for_test();
        self.sync_worker = worker;
        handle
    }

    /// Returns one retained dirty watermark and its directory coverage.
    #[cfg(test)]
    pub(crate) fn dirty_target(&self, path: &Path) -> Option<DirtyTargetSnapshot> {
        self.sync_worker.dirty_target(path)
    }
}

/// One deterministic failure plan for a test append.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AppendFault {
    /// Frame-byte offset that fails before that byte is written.
    pub fail_write_at: Option<usize>,
    /// Whether rollback truncation fails.
    pub fail_truncate: bool,
    /// Whether the EOF probe after a failed write also fails.
    pub fail_seek_after_write: bool,
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
}

#[cfg(test)]
impl<'a> FaultInjectingFile<'a> {
    /// Wraps `file` with `fault`.
    fn new(file: &'a mut File, fault: AppendFault) -> Self {
        Self {
            file,
            fault,
            written: 0,
        }
    }
}

#[cfg(test)]
impl FrameIo for FaultInjectingFile<'_> {
    fn seek_to_end(&mut self) -> io::Result<u64> {
        if 0 < self.written && self.fault.fail_seek_after_write {
            return Err(injected_error("post-write seek"));
        }
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
}

/// Creates a stable injected I/O error for assertions.
#[cfg(test)]
fn injected_error(operation: &str) -> io::Error {
    io::Error::other(format!("injected {operation} failure"))
}

/// Writes one complete `[u64 length][payload]` frame.
fn append_frame(io: &mut impl FrameIo, payload: &[u8]) -> Result<FrameAppend, FrameAppendError> {
    let start_offset = io.seek_to_end().map_err(|source| FrameAppendError {
        source,
        rollback: None,
    })?;
    let length = u64::try_from(payload.len())
        .expect("usize payload length always fits the journal's u64 frame prefix");
    let append_result = io
        .write_all(&length.to_le_bytes())
        .and_then(|()| io.write_all(payload));
    if let Err(source) = append_result {
        let rollback = match io.seek_to_end() {
            Ok(current_offset) if current_offset == start_offset => None,
            Ok(_) => io.truncate(start_offset).err(),
            Err(_) => io.truncate(start_offset).err(),
        };
        return Err(FrameAppendError { source, rollback });
    }
    Ok(FrameAppend {
        start_offset,
        end_offset: start_offset.saturating_add(8).saturating_add(length),
    })
}

/// Internal result from strict framed-stream recovery.
struct PrefixRecovery<T> {
    /// Validated records through clean EOF or before a repaired incomplete EOF
    /// tail.
    records: Vec<T>,
    /// Exact repaired EOF, or `None` when no repair was needed.
    truncated_to: Option<u64>,
}

fn recover_prefix<T, F>(path: &Path, mut validate: F) -> io::Result<PrefixRecovery<T>>
where
    T: DeserializeOwned,
    F: FnMut(&T) -> bool,
{
    let mut file = path_std_fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut records = Vec::new();
    loop {
        let frame_start = file.stream_position()?;
        let length = match read_record_length(&mut file) {
            Ok(Some(length)) if length <= MAX_RECORD_BYTES => length,
            Ok(None) => {
                return Ok(PrefixRecovery {
                    records,
                    truncated_to: None,
                });
            }
            Ok(Some(length)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "complete frame declares {length} bytes; maximum is {MAX_RECORD_BYTES}"
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                file.set_len(frame_start)?;
                return Ok(PrefixRecovery {
                    records,
                    truncated_to: Some(frame_start),
                });
            }
            Err(error) => return Err(error),
        };
        let mut bytes = vec![0_u8; length as usize];
        match file.read_exact(&mut bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                file.set_len(frame_start)?;
                return Ok(PrefixRecovery {
                    records,
                    truncated_to: Some(frame_start),
                });
            }
            Err(error) => return Err(error),
        }
        let mut cursor = io::Cursor::new(bytes.as_slice());
        let record = ciborium::from_reader(&mut cursor).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("complete frame failed typed decode: {error}"),
            )
        })?;
        if cursor.position() != length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "complete frame contains trailing payload bytes",
            ));
        }
        if !validate(&record) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "complete frame failed semantic validation",
            ));
        }
        records.push(record);
    }
}

/// Represents the current directory for paths whose parent is the empty
/// relative-path component.
fn normalized_directory(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        path.to_path_buf()
    }
}

/// Returns normalized parents from the immediate parent through `.` or `/`.
fn directory_ancestors(path: &Path) -> Vec<PathBuf> {
    let mut ancestors = Vec::new();
    let mut current = path.parent();
    while let Some(parent) = current {
        let normalized = normalized_directory(parent);
        let terminal =
            normalized == Path::new(".") || normalized.parent() == Some(normalized.as_path());
        ancestors.push(normalized);
        if terminal {
            break;
        }
        current = parent.parent();
    }
    ancestors
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

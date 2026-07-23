//! Persistent prompt input history.
//!
//! Entries are stored as length-prefixed CBOR records. Loading ignores
//! unreadable tail records, and appending first truncates torn or oversized
//! tail frames so newly appended prompts remain reachable after a crash. Load
//! and repair work are bounded by a total file-size cap; oversized history
//! files are ignored on load and discarded before writing the next prompt. A
//! process-local file-identity, offset, and final-up-to-64-byte boundary
//! witness lets warm appends validate only records added by another CLI since
//! the known-good prefix. Identity, length, or boundary mismatch falls back to
//! the complete bounded repair scan. Errors during locked validation, repair,
//! write, flush, or sync invalidate the witness; setup/lock errors before that
//! work leave it unchanged, and an unlock error after successful append keeps
//! the newly captured witness.
//! This cooperative same-user optimization detects ordinary replacement,
//! truncation, and tail mutation but is not tamper evidence. Framing, locking,
//! append-before-send ordering, repair, flush, and `sync_data` semantics remain
//! unchanged.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_config::settings::TauDirs;
use tau_proto::UnixMicros;

const HISTORY_FILE: &str = "prompt-history.cbor";
const LOCK_FILE: &str = "prompt-history.lock";
const MAX_HISTORY_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = MAX_HISTORY_FILE_BYTES;
const MAX_PROMPT_HISTORY_ENTRIES: usize = 1000;
// Keep at zero per `DECISION-no-backward-compatibility`.
const PROMPT_HISTORY_VERSION: u8 = 0;

/// Persistent prompt-history access with a shared process-local tail witness.
#[derive(Clone)]
pub(crate) struct PromptHistoryStore {
    /// Global prompt-history path, or none when state persistence is
    /// unavailable.
    path: Option<PathBuf>,
    /// Last prefix validated by this process's clones.
    validated_tail: Arc<Mutex<Option<ValidatedTail>>>,
}

/// Identity and boundary witness for one validated append-only file prefix.
///
/// The witness has authority only after device, inode, length, and `boundary`
/// all match under the cross-process history lock. A mismatch falls back to
/// validating from byte zero.
#[derive(Clone, Eq, PartialEq)]
struct ValidatedTail {
    /// Device containing the validated file.
    device: u64,
    /// Inode of the validated file.
    inode: u64,
    /// First byte not covered by validation.
    end_offset: u64,
    /// Last bytes ending exactly at `end_offset`.
    boundary: Vec<u8>,
    /// Records visited by the append that produced this witness.
    #[cfg(test)]
    records_scanned: usize,
}

/// Result of validating one possibly empty framed-file suffix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TailValidation {
    /// First byte after the validated framed prefix.
    end_offset: u64,
    /// Framed records visited after the trusted starting offset.
    records_scanned: usize,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PromptHistoryRecord {
    version: u8,
    recorded_at_micros: u64,
    text: String,
}

impl PromptHistoryStore {
    #[must_use]
    pub(crate) fn new(dirs: &TauDirs) -> Self {
        Self {
            path: dirs.state_dir.as_ref().map(|dir| dir.join(HISTORY_FILE)),
            validated_tail: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    fn for_path(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            validated_tail: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn load(&self) -> io::Result<Vec<String>> {
        let Some(path) = self.path.as_deref() else {
            return Ok(Vec::new());
        };
        let (entries, validated_tail) = load_prompt_history(path)?;
        *self
            .validated_tail
            .lock()
            .expect("prompt-history tail mutex poisoned") = validated_tail;
        Ok(entries)
    }

    pub(crate) fn append(&self, text: &str) -> io::Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        if text.is_empty() {
            return Ok(());
        }
        append_prompt_history(path, text, &self.validated_tail)
    }
}

fn load_prompt_history(path: &Path) -> io::Result<(Vec<String>, Option<ValidatedTail>)> {
    let Some(parent) = path.parent() else {
        return Ok((Vec::new(), None));
    };
    fs::create_dir_all(parent)?;
    let lock_file = open_lock_file(parent)?;
    FileExt::lock_shared(&lock_file)?;
    let result = load_prompt_history_locked(path);
    let unlock_result = FileExt::unlock(&lock_file);
    match (result, unlock_result) {
        (Ok(entries), Ok(())) => Ok(entries),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn load_prompt_history_locked(path: &Path) -> io::Result<(Vec<String>, Option<ValidatedTail>)> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((Vec::new(), None));
        }
        Err(error) => return Err(error),
    };
    if MAX_HISTORY_FILE_BYTES < file.metadata()?.len() {
        tracing::warn!(
            target: "tau_cli::prompt_history",
            path = %path.display(),
            max_file_bytes = MAX_HISTORY_FILE_BYTES,
            "ignoring oversized prompt-history file"
        );
        return Ok((Vec::new(), None));
    }
    let mut entries = VecDeque::new();
    let mut valid_len = 0_u64;
    loop {
        let mut length_bytes = [0_u8; 8];
        match file.read_exact(&mut length_bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                tracing::warn!(
                    target: "tau_cli::prompt_history",
                    path = %path.display(),
                    "ignoring truncated prompt-history length header"
                );
                break;
            }
            Err(error) => return Err(error),
        }
        let record_length = u64::from_le_bytes(length_bytes);
        if MAX_RECORD_BYTES < record_length {
            tracing::warn!(
                target: "tau_cli::prompt_history",
                path = %path.display(),
                record_length,
                max_record_bytes = MAX_RECORD_BYTES,
                "ignoring corrupt prompt-history tail with oversized record"
            );
            break;
        }
        let mut record_bytes = vec![0_u8; record_length as usize];
        match file.read_exact(&mut record_bytes) {
            Ok(()) => {
                valid_len = valid_len.saturating_add(8).saturating_add(record_length);
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                tracing::warn!(
                    target: "tau_cli::prompt_history",
                    path = %path.display(),
                    record_length,
                    "ignoring truncated prompt-history tail record"
                );
                break;
            }
            Err(error) => return Err(error),
        }
        let record: PromptHistoryRecord = match ciborium::from_reader(record_bytes.as_slice()) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(
                    target: "tau_cli::prompt_history",
                    path = %path.display(),
                    %error,
                    record_length,
                    "ignoring malformed prompt-history record"
                );
                continue;
            }
        };
        if record.version != PROMPT_HISTORY_VERSION {
            tracing::warn!(
                target: "tau_cli::prompt_history",
                path = %path.display(),
                version = record.version,
                "ignoring unsupported prompt-history record"
            );
            continue;
        }
        if record.text.is_empty() {
            continue;
        }
        if MAX_PROMPT_HISTORY_ENTRIES <= entries.len() {
            entries.pop_front();
        }
        entries.push_back(record.text);
    }
    let validated_tail = ValidatedTail::capture_at(&mut file, valid_len)?;
    Ok((entries.into_iter().collect(), Some(validated_tail)))
}

fn append_prompt_history(
    path: &Path,
    text: &str,
    validated_tail: &Mutex<Option<ValidatedTail>>,
) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    let lock_file = open_lock_file(parent)?;
    FileExt::lock_exclusive(&lock_file)?;
    let previous_tail = validated_tail
        .lock()
        .expect("prompt-history tail mutex poisoned")
        .clone();
    let result = append_prompt_history_locked_with_tail(path, text, previous_tail.as_ref());
    let mut cached_tail = validated_tail
        .lock()
        .expect("prompt-history tail mutex poisoned");
    match &result {
        Ok(new_tail) => *cached_tail = Some(new_tail.clone()),
        Err(_) => *cached_tail = None,
    }
    let unlock_result = FileExt::unlock(&lock_file);
    result.map(|_| ()).and(unlock_result)
}

#[cfg(test)]
fn append_prompt_history_locked_with_limit(
    path: &Path,
    text: &str,
    max_file_bytes: u64,
) -> io::Result<ValidatedTail> {
    append_prompt_history_locked_with_tail_and_limit(path, text, None, max_file_bytes)
}

fn append_prompt_history_locked_with_tail(
    path: &Path,
    text: &str,
    validated_tail: Option<&ValidatedTail>,
) -> io::Result<ValidatedTail> {
    append_prompt_history_locked_with_tail_and_limit(
        path,
        text,
        validated_tail,
        MAX_HISTORY_FILE_BYTES,
    )
}

fn append_prompt_history_locked_with_tail_and_limit(
    path: &Path,
    text: &str,
    validated_tail: Option<&ValidatedTail>,
    max_file_bytes: u64,
) -> io::Result<ValidatedTail> {
    let record = PromptHistoryRecord {
        version: PROMPT_HISTORY_VERSION,
        recorded_at_micros: UnixMicros::now().get(),
        text: text.to_owned(),
    };
    let mut encoded = Vec::new();
    ciborium::into_writer(&record, &mut encoded)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let framed_len = 8_u64
        .checked_add(encoded.len() as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "record length overflowed"))?;
    if max_file_bytes < framed_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("prompt-history framed record length {framed_len} exceeds {max_file_bytes}"),
        ));
    }
    let encoded_len = encoded.len() as u64;
    if MAX_RECORD_BYTES < encoded_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("prompt-history record length {encoded_len} exceeds {MAX_RECORD_BYTES}"),
        ));
    }

    let mut entry = Vec::with_capacity(8 + encoded.len());
    entry.extend_from_slice(&encoded_len.to_le_bytes());
    entry.extend_from_slice(&encoded);

    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    let validation_start = validated_tail
        .filter(|tail| tail.matches(&mut file).unwrap_or(false))
        .map_or(0, |tail| tail.end_offset);
    let validation = truncate_corrupt_prompt_history_tail_from_with_limit(
        path,
        &mut file,
        max_file_bytes,
        validation_start,
    )?;
    let current_len = validation.end_offset;
    if max_file_bytes.saturating_sub(current_len) < framed_len {
        tracing::warn!(
            target: "tau_cli::prompt_history",
            path = %path.display(),
            current_len,
            framed_len,
            max_file_bytes,
            "truncating prompt-history file before append would exceed size limit"
        );
        file.set_len(0)?;
    }
    file.seek(io::SeekFrom::End(0))?;
    file.write_all(&entry)?;
    file.flush()?;
    file.sync_data()?;
    let tail = ValidatedTail::capture(&mut file)?;
    #[cfg(test)]
    let tail = {
        let mut tail = tail;
        tail.records_scanned = validation.records_scanned;
        tail
    };
    Ok(tail)
}

impl ValidatedTail {
    fn matches(&self, file: &mut File) -> io::Result<bool> {
        let metadata = file.metadata()?;
        if metadata.dev() != self.device
            || metadata.ino() != self.inode
            || metadata.len() < self.end_offset
            || self.end_offset < self.boundary.len() as u64
        {
            return Ok(false);
        }
        let boundary_start = self.end_offset - self.boundary.len() as u64;
        file.seek(io::SeekFrom::Start(boundary_start))?;
        let mut boundary = vec![0_u8; self.boundary.len()];
        file.read_exact(&mut boundary)?;
        Ok(boundary == self.boundary)
    }

    fn capture(file: &mut File) -> io::Result<Self> {
        let end_offset = file.seek(io::SeekFrom::End(0))?;
        Self::capture_at(file, end_offset)
    }

    fn capture_at(file: &mut File, end_offset: u64) -> io::Result<Self> {
        const BOUNDARY_BYTES: u64 = 64;

        let metadata = file.metadata()?;
        if metadata.len() < end_offset {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "validated prompt-history offset exceeds file length",
            ));
        }
        let boundary_start = end_offset.saturating_sub(BOUNDARY_BYTES);
        file.seek(io::SeekFrom::Start(boundary_start))?;
        let mut boundary = vec![0_u8; (end_offset - boundary_start) as usize];
        file.read_exact(&mut boundary)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            end_offset,
            boundary,
            #[cfg(test)]
            records_scanned: 0,
        })
    }
}

#[cfg(test)]
fn truncate_corrupt_prompt_history_tail_with_limit(
    path: &Path,
    file: &mut File,
    max_file_bytes: u64,
) -> io::Result<TailValidation> {
    truncate_corrupt_prompt_history_tail_from_with_limit(path, file, max_file_bytes, 0)
}

fn truncate_corrupt_prompt_history_tail_from_with_limit(
    path: &Path,
    file: &mut File,
    max_file_bytes: u64,
    validation_start: u64,
) -> io::Result<TailValidation> {
    let file_len = file.metadata()?.len();
    if max_file_bytes < file_len {
        tracing::warn!(
            target: "tau_cli::prompt_history",
            path = %path.display(),
            max_file_bytes,
            "truncating oversized prompt-history file before appending"
        );
        file.set_len(0)?;
        return Ok(TailValidation {
            end_offset: 0,
            records_scanned: 0,
        });
    }

    let mut valid_len = validation_start.min(file_len);
    file.seek(io::SeekFrom::Start(valid_len))?;
    if valid_len == file_len {
        return Ok(TailValidation {
            end_offset: valid_len,
            records_scanned: 0,
        });
    }
    let mut records_scanned = 0_usize;
    loop {
        let record_start = valid_len;
        let mut length_bytes = [0_u8; 8];
        match file.read_exact(&mut length_bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                truncate_prompt_history_file(path, file, record_start, "partial length header")?;
                return Ok(TailValidation {
                    end_offset: record_start,
                    records_scanned,
                });
            }
            Err(error) => return Err(error),
        }

        let record_length = u64::from_le_bytes(length_bytes);
        if MAX_RECORD_BYTES < record_length {
            truncate_prompt_history_file(path, file, record_start, "oversized record")?;
            return Ok(TailValidation {
                end_offset: record_start,
                records_scanned,
            });
        }

        let record_end = record_start
            .checked_add(8)
            .and_then(|offset| offset.checked_add(record_length))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "prompt-history record offset overflowed",
                )
            })?;
        match file.seek(io::SeekFrom::Start(record_end)) {
            Ok(_) => {
                valid_len = record_end;
                records_scanned = records_scanned.saturating_add(1);
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                truncate_prompt_history_file(path, file, record_start, "invalid record offset")?;
                return Ok(TailValidation {
                    end_offset: record_start,
                    records_scanned,
                });
            }
            Err(error) => return Err(error),
        }

        if file_len < record_end {
            truncate_prompt_history_file(path, file, record_start, "partial record payload")?;
            return Ok(TailValidation {
                end_offset: record_start,
                records_scanned,
            });
        }
        if file_len == record_end {
            return Ok(TailValidation {
                end_offset: record_end,
                records_scanned,
            });
        }
    }
}

fn truncate_prompt_history_file(
    path: &Path,
    file: &mut File,
    valid_len: u64,
    reason: &str,
) -> io::Result<()> {
    tracing::warn!(
        target: "tau_cli::prompt_history",
        path = %path.display(),
        valid_len,
        reason,
        "truncating corrupt prompt-history tail before appending"
    );
    file.set_len(valid_len)?;
    file.seek(io::SeekFrom::Start(valid_len))?;
    Ok(())
}

fn open_lock_file(parent: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(parent.join(LOCK_FILE))
}

#[cfg(test)]
mod tests;

//! Persistent prompt input history.
//!
//! Entries are stored as length-prefixed CBOR records. Loading ignores
//! unreadable tail records, and the persistence worker first truncates torn or
//! oversized tail frames so newly appended prompts remain reachable after a
//! crash. Load and repair work are bounded by a total file-size cap; oversized
//! history files are ignored on load and discarded before writing the next
//! prompt. A process-local file-identity, offset, and final-up-to-64-byte
//! boundary witness lets warm appends validate only records added by another
//! CLI since the known-good prefix. Identity, length, or boundary mismatch
//! falls back to the complete bounded repair scan. Errors during locked
//! validation, repair, or write invalidate the witness; setup/lock errors
//! before that work leave it unchanged, and an unlock error after successful
//! append keeps the newly captured witness.
//! This cooperative same-user optimization detects ordinary replacement,
//! truncation, and tail mutation but is not tamper evidence. Framing, locking,
//! and repair preserve safe multi-process appends where possible. Submission
//! queues persistence without waiting for filesystem work or queue capacity: a
//! full queue drops the newest entry. The worker neither flushes nor calls
//! `sync_data`, and shutdown never waits for it to drain.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::Builder;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_config::settings::TauDirs;
use tau_proto::UnixMicros;

const HISTORY_FILE: &str = "prompt-history.cbor";
const LOCK_FILE: &str = "prompt-history.lock";
const MAX_HISTORY_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = MAX_HISTORY_FILE_BYTES;
const MAX_PROMPT_HISTORY_ENTRIES: usize = 1000;
const PERSIST_QUEUE_CAPACITY: usize = 64;
const PERSIST_QUEUE_MAX_BYTES: usize = 1024 * 1024;
// Keep at zero per `GATE-no-backward-compatibility`.
const PROMPT_HISTORY_VERSION: u8 = 0;

/// Persistent prompt-history access with bounded best-effort admission.
///
/// [`Self::append`] preserves submission order only among entries accepted by
/// its bounded FIFO. It allocates an owned copy only after byte admission,
/// never waits for queue space or filesystem I/O, and returns a drop outcome
/// rather than durability. The detached worker does not drain on shutdown.
#[derive(Clone)]
pub(crate) struct PromptHistoryStore {
    /// Global prompt-history path, or none when state persistence is
    /// unavailable.
    path: Option<PathBuf>,
    /// Last prefix validated by this process's clones.
    validated_tail: Arc<Mutex<Option<ValidatedTail>>>,
    /// Bounded asynchronous persistence queue. A full queue drops its newest
    /// requested history entry rather than delaying prompt submission.
    persistence_tx: Option<SyncSender<PromptHistoryWrite>>,
    /// Total bytes held by queued and in-flight persistence requests.
    queued_bytes: Arc<AtomicUsize>,
}

/// One best-effort prompt-history persistence request.
enum PromptHistoryWrite {
    /// Append one submitted prompt to persistent history.
    Append {
        /// Submitted prompt text retained until the worker finishes it.
        text: String,
        /// Bytes reserved from the shared queued-and-in-flight budget.
        queue_bytes: usize,
    },
    /// Test-only ordering point after all earlier persistence requests.
    #[cfg(test)]
    Barrier(SyncSender<()>),
}

impl PromptHistoryWrite {
    /// Returns bytes retained by this request under the shared memory budget.
    fn queue_bytes(&self) -> usize {
        match self {
            Self::Append { queue_bytes, .. } => *queue_bytes,
            #[cfg(test)]
            Self::Barrier(_) => 0,
        }
    }
}

/// Outcome of admitting one submitted prompt to best-effort persistence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptHistoryAdmission {
    /// The worker accepted the prompt for later persistence.
    Queued,
    /// Empty prompt text does not require persistent history.
    IgnoredEmpty,
    /// The bounded queue was full, so the newest prompt was deliberately lost.
    DroppedFull,
    /// The persistence worker is unavailable, so the prompt was deliberately
    /// lost.
    DroppedUnavailable,
}

impl PromptHistoryAdmission {
    /// Returns the bounded, content-free diagnostic class for this admission.
    pub(crate) const fn diagnostic_class(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::IgnoredEmpty => "ignored_empty",
            Self::DroppedFull => "dropped_full",
            Self::DroppedUnavailable => "dropped_unavailable",
        }
    }
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
        Self::for_optional_path(dirs.state_dir.as_ref().map(|dir| dir.join(HISTORY_FILE)))
    }

    #[cfg(test)]
    fn for_path(path: PathBuf) -> Self {
        Self::for_optional_path(Some(path))
    }

    #[cfg(test)]
    fn for_path_without_worker(path: PathBuf) -> (Self, Receiver<PromptHistoryWrite>) {
        let validated_tail = Arc::new(Mutex::new(None));
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let (persistence_tx, persistence_rx) = mpsc::sync_channel(PERSIST_QUEUE_CAPACITY);
        (
            Self {
                path: Some(path),
                validated_tail,
                persistence_tx: Some(persistence_tx),
                queued_bytes,
            },
            persistence_rx,
        )
    }

    fn for_optional_path(path: Option<PathBuf>) -> Self {
        let validated_tail = Arc::new(Mutex::new(None));
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let persistence_tx = path.as_ref().and_then(|path| {
            start_prompt_history_persistence_worker(
                path.clone(),
                validated_tail.clone(),
                queued_bytes.clone(),
            )
        });
        Self {
            path,
            validated_tail,
            persistence_tx,
            queued_bytes,
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

    /// Attempts nonblocking best-effort persistence for one submitted prompt.
    ///
    /// This reserves bounded queue bytes before cloning `text`, then returns
    /// immediately after FIFO admission or a deliberate newest-entry drop.
    /// [`PromptHistoryAdmission::Queued`] only means the worker owns a copy;
    /// it does not mean the record reached the filesystem or is durable.
    pub(crate) fn append(&self, text: &str) -> PromptHistoryAdmission {
        if text.is_empty() {
            return PromptHistoryAdmission::IgnoredEmpty;
        }
        let Some(persistence_tx) = &self.persistence_tx else {
            return PromptHistoryAdmission::DroppedUnavailable;
        };
        let queue_bytes = text.len();
        if !reserve_prompt_history_queue_bytes(&self.queued_bytes, queue_bytes) {
            return PromptHistoryAdmission::DroppedFull;
        }
        let request = PromptHistoryWrite::Append {
            text: text.to_owned(),
            queue_bytes,
        };
        match persistence_tx.try_send(request) {
            Ok(()) => PromptHistoryAdmission::Queued,
            Err(TrySendError::Full(request)) => {
                release_prompt_history_queue_bytes(&self.queued_bytes, request.queue_bytes());
                PromptHistoryAdmission::DroppedFull
            }
            Err(TrySendError::Disconnected(request)) => {
                release_prompt_history_queue_bytes(&self.queued_bytes, request.queue_bytes());
                PromptHistoryAdmission::DroppedUnavailable
            }
        }
    }

    #[cfg(test)]
    fn append_and_wait(&self, text: &str) {
        assert_eq!(
            self.append(text),
            PromptHistoryAdmission::Queued,
            "test history append must enter the persistence queue"
        );
        self.wait_for_persistence();
    }

    #[cfg(test)]
    fn wait_for_persistence(&self) {
        let Some(persistence_tx) = &self.persistence_tx else {
            panic!("test history store must have a persistence worker");
        };
        let (barrier_tx, barrier_rx) = mpsc::sync_channel(0);
        persistence_tx
            .send(PromptHistoryWrite::Barrier(barrier_tx))
            .expect("test history persistence worker must remain available");
        barrier_rx
            .recv()
            .expect("test history persistence worker must cross barrier");
    }
}

/// Reserves request bytes without exceeding the queue-and-worker memory budget.
fn reserve_prompt_history_queue_bytes(queued_bytes: &AtomicUsize, bytes: usize) -> bool {
    let mut current = queued_bytes.load(Ordering::Relaxed);
    loop {
        if PERSIST_QUEUE_MAX_BYTES.saturating_sub(current) < bytes {
            return false;
        }
        match queued_bytes.compare_exchange_weak(
            current,
            current.saturating_add(bytes),
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// Releases bytes retained by a request that was rejected or has finished
/// writing.
fn release_prompt_history_queue_bytes(queued_bytes: &AtomicUsize, queue_bytes: usize) {
    queued_bytes.fetch_sub(queue_bytes, Ordering::Release);
}

/// Starts the best-effort history worker and returns its bounded admission
/// queue.
fn start_prompt_history_persistence_worker(
    path: PathBuf,
    validated_tail: Arc<Mutex<Option<ValidatedTail>>>,
    queued_bytes: Arc<AtomicUsize>,
) -> Option<SyncSender<PromptHistoryWrite>> {
    let (persistence_tx, persistence_rx) = mpsc::sync_channel(PERSIST_QUEUE_CAPACITY);
    match Builder::new()
        .name("tau-prompt-history".to_owned())
        .spawn(move || {
            prompt_history_persistence_loop(path, validated_tail, queued_bytes, persistence_rx)
        }) {
        Ok(_) => Some(persistence_tx),
        Err(error) => {
            tracing::warn!(
                target: "tau_cli::prompt_history",
                %error,
                "could not start best-effort prompt-history persistence worker"
            );
            None
        }
    }
}

/// Persists queued history entries without imposing durability waits on
/// callers.
fn prompt_history_persistence_loop(
    path: PathBuf,
    validated_tail: Arc<Mutex<Option<ValidatedTail>>>,
    queued_bytes: Arc<AtomicUsize>,
    persistence_rx: Receiver<PromptHistoryWrite>,
) {
    while let Ok(request) = persistence_rx.recv() {
        match request {
            PromptHistoryWrite::Append { text, queue_bytes } => {
                if let Err(error) = append_prompt_history(&path, &text, &validated_tail) {
                    tracing::warn!(
                        target: "tau_cli::prompt_history",
                        %error,
                        "failed to persist queued prompt history"
                    );
                }
                release_prompt_history_queue_bytes(&queued_bytes, queue_bytes);
            }
            #[cfg(test)]
            PromptHistoryWrite::Barrier(barrier_tx) => {
                let _ = barrier_tx.send(());
            }
        }
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

//! Persistent prompt input history.
//!
//! Entries are stored as length-prefixed CBOR records. Loading ignores
//! unreadable tail records, and appending first truncates torn or oversized
//! tail frames so newly appended prompts remain reachable after a crash. Load
//! and repair work are bounded by a total file-size cap; oversized history
//! files are ignored on load and discarded before writing the next prompt.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

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

#[derive(Clone, Debug)]
pub(crate) struct PromptHistoryStore {
    path: Option<PathBuf>,
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
        }
    }

    pub(crate) fn load(&self) -> io::Result<Vec<String>> {
        let Some(path) = self.path.as_deref() else {
            return Ok(Vec::new());
        };
        load_prompt_history(path)
    }

    pub(crate) fn append(&self, text: &str) -> io::Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        if text.is_empty() {
            return Ok(());
        }
        append_prompt_history(path, text)
    }
}

fn load_prompt_history(path: &Path) -> io::Result<Vec<String>> {
    let Some(parent) = path.parent() else {
        return Ok(Vec::new());
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

fn load_prompt_history_locked(path: &Path) -> io::Result<Vec<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if MAX_HISTORY_FILE_BYTES < file.metadata()?.len() {
        tracing::warn!(
            target: "tau_cli::prompt_history",
            path = %path.display(),
            max_file_bytes = MAX_HISTORY_FILE_BYTES,
            "ignoring oversized prompt-history file"
        );
        return Ok(Vec::new());
    }
    let mut entries = VecDeque::new();
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
            Ok(()) => {}
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
    Ok(entries.into_iter().collect())
}

fn append_prompt_history(path: &Path, text: &str) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    let lock_file = open_lock_file(parent)?;
    FileExt::lock_exclusive(&lock_file)?;
    let result = append_prompt_history_locked(path, text);
    let unlock_result = FileExt::unlock(&lock_file);
    result.and(unlock_result)
}

fn append_prompt_history_locked(path: &Path, text: &str) -> io::Result<()> {
    append_prompt_history_locked_with_limit(path, text, MAX_HISTORY_FILE_BYTES)
}

fn append_prompt_history_locked_with_limit(
    path: &Path,
    text: &str,
    max_file_bytes: u64,
) -> io::Result<()> {
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
    truncate_corrupt_prompt_history_tail_with_limit(path, &mut file, max_file_bytes)?;
    let current_len = file.metadata()?.len();
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
    file.sync_data()
}

fn truncate_corrupt_prompt_history_tail_with_limit(
    path: &Path,
    file: &mut File,
    max_file_bytes: u64,
) -> io::Result<()> {
    if max_file_bytes < file.metadata()?.len() {
        tracing::warn!(
            target: "tau_cli::prompt_history",
            path = %path.display(),
            max_file_bytes,
            "truncating oversized prompt-history file before appending"
        );
        file.set_len(0)?;
    }

    file.seek(io::SeekFrom::Start(0))?;
    let mut valid_len = 0_u64;
    loop {
        let record_start = valid_len;
        let mut length_bytes = [0_u8; 8];
        match file.read_exact(&mut length_bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                truncate_prompt_history_file(path, file, record_start, "partial length header")?;
                return Ok(());
            }
            Err(error) => return Err(error),
        }

        let record_length = u64::from_le_bytes(length_bytes);
        if MAX_RECORD_BYTES < record_length {
            truncate_prompt_history_file(path, file, record_start, "oversized record")?;
            return Ok(());
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
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                truncate_prompt_history_file(path, file, record_start, "invalid record offset")?;
                return Ok(());
            }
            Err(error) => return Err(error),
        }

        let file_len = file.metadata()?.len();
        if file_len < record_end {
            truncate_prompt_history_file(path, file, record_start, "partial record payload")?;
            return Ok(());
        }
        if file_len == record_end {
            return Ok(());
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

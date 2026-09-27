//! Local inspection and archival clearing for the standard papercut reporter.
//!
//! The reporter owns the JSONL records in its User-scope extension directory.
//! This module reads that canonical file directly and takes the same advisory
//! directory lock as the harness before archiving it.

use std::fs as path_std_fs;
use std::io::{Error as IoError, ErrorKind, Read as _};
use std::path::{Path, PathBuf};

use fs2::FileExt as _;
use tau_ext_utils::{PAPERCUT_FILE_NAME, PapercutRecord, papercut_history};

use crate::cli::PapercutCommand;
use crate::{CliError, line_output};

/// Configured instance name for Tau's standard utility extension.
const STD_UTILS_INSTANCE: &str = "std-utils";
/// Prefix for preserved files removed from the active papercut listing.
const PAPERCUT_ARCHIVE_PREFIX: &str = tau_proto::PAPERCUT_ARCHIVE_PREFIX;

/// Runs one `tau dev papercut` command.
pub(crate) fn run(command: PapercutCommand) -> Result<(), CliError> {
    match command {
        PapercutCommand::List {
            markdown,
            state_dir,
        } => {
            let records = PapercutStore::new(&state_dir).list()?;
            let output = if markdown {
                format_markdown(&records)?
            } else {
                format_plain(&records)?
            };
            line_output::write_stdout(&output)
        }
        PapercutCommand::Clear { state_dir } => {
            let result = PapercutStore::new(&state_dir).clear()?;
            line_output::write_stdout(&format_clear_result(&result))
        }
    }
}

/// Result of removing one locked papercut snapshot from the active listing.
#[derive(Debug, Eq, PartialEq)]
struct PapercutClearResult {
    /// Number of validated reports preserved in the archived snapshot.
    count: usize,
    /// New archive path, absent when there was no active records file.
    archive: Option<PathBuf>,
}

/// Canonical User-scope storage owned by one standard papercut reporter
/// instance.
#[derive(Clone)]
struct PapercutStore {
    /// Existing Tau state root selected by the caller.
    root: PathBuf,
    /// Deterministic test-only boundary after archival and before lock release.
    #[cfg(test)]
    clear_midpoint: Option<std::sync::Arc<std::sync::Barrier>>,
}

impl PapercutStore {
    /// Constructs the standard reporter's canonical User-scope storage paths.
    fn new(state_dir: &Path) -> Self {
        let root = tau_config::settings::extension_state_dir_of(state_dir, STD_UTILS_INSTANCE)
            .expect("built-in extension instance name must be valid");
        Self {
            root,
            #[cfg(test)]
            clear_midpoint: None,
        }
    }

    /// Returns the reporter-owned JSONL path below this store's locked root.
    fn file(&self) -> PathBuf {
        self.root.join(PAPERCUT_FILE_NAME)
    }

    /// Lists records from one lock-consistent snapshot, oldest timestamp first.
    fn list(&self) -> Result<Vec<PapercutRecord>, CliError> {
        self.with_existing_lock(|| self.read_records())
            .map(|records| records.unwrap_or_default())
    }

    /// Archives exactly the records present while holding the reporter's append
    /// lock, removing them from the active listing without deleting their
    /// bytes.
    fn clear(&self) -> Result<PapercutClearResult, CliError> {
        self.with_existing_lock(|| {
            let records = self.read_records()?;
            let file = self.file();
            let archive = if file.exists() {
                let archive = self.archive_active_file(&file)?;
                sync_parent_dir(&file)?;
                Some(archive)
            } else {
                None
            };
            #[cfg(test)]
            self.wait_at_clear_midpoint();
            Ok(PapercutClearResult {
                count: records.len(),
                archive,
            })
        })
        .map(|result| {
            result.unwrap_or(PapercutClearResult {
                count: 0,
                archive: None,
            })
        })
    }

    /// Renames the active file to the first unused, lexically ordered archive
    /// path without replacing an existing archive.
    fn archive_active_file(&self, file: &Path) -> Result<PathBuf, CliError> {
        for index in 1_u64.. {
            let candidate = self
                .root
                .join(format!("{PAPERCUT_ARCHIVE_PREFIX}{index:016}.jsonl"));
            if archive_path_is_occupied(&candidate)
                .map_err(|error| storage_error("failed to inspect papercut archive path", error))?
            {
                continue;
            }
            match rename_no_replace(file, &candidate) {
                Ok(()) => return Ok(candidate),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(storage_error("failed to clear the papercut records", error));
                }
            }
        }
        unreachable!("u64 archive namespace cannot be exhausted in practice")
    }

    /// Runs one read or mutation while sharing the reporter's exclusive
    /// directory lock.
    ///
    /// An absent extension directory means no reporter has created storage yet.
    fn with_existing_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, CliError>,
    ) -> Result<Option<T>, CliError> {
        let root = match open_existing_directory_no_follow(&self.root) {
            Ok(root) => root,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage_error("failed to open papercut storage", error)),
        };
        root.lock_exclusive()
            .map_err(|error| storage_error("failed to lock papercut storage", error))?;
        let result = operation();
        let _ = root.unlock();
        result.map(Some)
    }

    /// Parses and stably orders every supported record in the canonical file.
    fn read_records(&self) -> Result<Vec<PapercutRecord>, CliError> {
        self.read_records_from(&self.file())
    }

    /// Parses and stably orders every supported record in one records file.
    fn read_records_from(&self, file_path: &Path) -> Result<Vec<PapercutRecord>, CliError> {
        let mut file = match open_existing_file_no_follow(file_path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(storage_error("failed to open papercut records", error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| storage_error("failed to inspect papercut records", error))?;
        if !metadata.is_file() {
            return Err(CliError::Participant(
                "papercut records are not a regular file".to_owned(),
            ));
        }
        let mut contents = Vec::new();
        file.by_ref()
            .take(tau_harness::EXTENSION_DATA_MAX_FILE_BYTES + 1)
            .read_to_end(&mut contents)
            .map_err(|error| storage_error("failed to read papercut records", error))?;
        if tau_harness::EXTENSION_DATA_MAX_FILE_BYTES < contents.len() as u64 {
            return Err(CliError::Participant(
                "papercut records exceed the extension data file limit".to_owned(),
            ));
        }
        papercut_history::parse_records(&contents).map_err(CliError::Participant)
    }

    /// Synchronizes a deterministic test at the post-removal clear boundary.
    #[cfg(test)]
    fn wait_at_clear_midpoint(&self) {
        if let Some(midpoint) = &self.clear_midpoint {
            midpoint.wait();
            midpoint.wait();
        }
    }

    /// Configures a test-only pause after archival while clear still owns the
    /// lock.
    #[cfg(test)]
    fn with_clear_midpoint(mut self, midpoint: std::sync::Arc<std::sync::Barrier>) -> Self {
        self.clear_midpoint = Some(midpoint);
        self
    }
}

/// Formats the archival result while retaining the clear command's record
/// count and surfacing the preserved file for later recovery.
fn format_clear_result(result: &PapercutClearResult) -> String {
    match &result.archive {
        Some(archive) => format!(
            "cleared {} papercut report(s); archived at {}\n",
            result.count,
            archive.display()
        ),
        None => "cleared 0 papercut report(s); no archive created\n".to_owned(),
    }
}

/// Formats the same line-oriented representation as the shared reporter reader.
fn format_plain(records: &[PapercutRecord]) -> Result<String, CliError> {
    papercut_history::format_plain(records).map_err(CliError::Participant)
}

/// Formats the same copyable Markdown representation as the shared reader.
fn format_markdown(records: &[PapercutRecord]) -> Result<String, CliError> {
    papercut_history::format_markdown(records).map_err(CliError::Participant)
}

/// Opens an existing extension data root without following a final symlink.
#[cfg(unix)]
fn open_existing_directory_no_follow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt as _;

    path_std_fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

/// Opens an existing records file without following a final symlink.
#[cfg(unix)]
fn open_existing_file_no_follow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt as _;

    path_std_fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Opens an existing records file on platforms without no-follow flags.
#[cfg(not(unix))]
fn open_existing_file_no_follow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    path_std_fs::File::open(path)
}

/// Opens an existing extension data root on platforms without no-follow flags.
#[cfg(not(unix))]
fn open_existing_directory_no_follow(path: &Path) -> Result<std::fs::File, std::io::Error> {
    path_std_fs::File::open(path)
}

/// Returns whether any filesystem entry, including a dangling symlink, already
/// occupies one candidate archive path.
fn archive_path_is_occupied(path: &Path) -> Result<bool, IoError> {
    match path_std_fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Atomically renames one file while refusing to replace an existing
/// destination on Linux.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_no_replace(source: &Path, destination: &Path) -> Result<(), IoError> {
    use rustix_v1::fs::{CWD, RenameFlags, renameat_with};

    renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE).map_err(IoError::from)
}

/// Atomically renames one file to an unused destination on platforms without
/// Linux's no-replace rename flag.
///
/// The caller's shared papercut directory lock serializes supported writers.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn rename_no_replace(source: &Path, destination: &Path) -> Result<(), IoError> {
    path_std_fs::rename(source, destination)
}

/// Flushes the parent directory after renaming the canonical records file.
fn sync_parent_dir(path: &Path) -> Result<(), CliError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    path_std_fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| storage_error("failed to persist archiving papercut records", error))
}

/// Converts local canonical-storage I/O failures into one CLI diagnostic.
fn storage_error(context: &str, error: std::io::Error) -> CliError {
    CliError::Participant(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests;

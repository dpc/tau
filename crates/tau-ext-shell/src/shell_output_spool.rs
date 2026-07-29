//! Ephemeral storage for ext-shell output that exceeds a visible budget.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// Maximum rendered shell output retained in a saved artifact.
pub(crate) const MAX_SAVED_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_LATER_CALLS: u64 = 32;
const MAX_AGE: Duration = Duration::from_secs(15 * 60);
const DIRECTORY_PREFIX: &str = "tau-shell-output-";
const FILE_NAME: &str = "output";
const LOCK_FILE_NAME: &str = "owner.lock";

/// One artifact owned by the live extension process.
struct SavedFile {
    /// Exact artifact path.
    path: PathBuf,
    /// Wall-clock creation time used for age expiry.
    created: SystemTime,
    /// Relevant ext-shell execution sequence at creation.
    call: u64,
    /// Open lock proving that this process still owns the directory.
    _owner_lock: fs::File,
}

/// Process-wide lifecycle state for ephemeral shell artifacts.
#[derive(Default)]
struct Tracker {
    /// Number of relevant ext-shell executions admitted to dispatch.
    call: u64,
    /// Artifacts awaiting expiry or shutdown cleanup.
    files: Vec<SavedFile>,
    /// Whether first-call crash-leftover cleanup has run.
    scanned_leftovers: bool,
}

static TRACKER: OnceLock<Mutex<Tracker>> = OnceLock::new();

/// Saved artifact metadata returned to a model or user-shell result.
pub(crate) struct SavedOutput {
    /// Exact path to the readable artifact.
    pub(crate) path: PathBuf,
    /// Whether the artifact itself stopped at the hard saved-output cap.
    pub(crate) incomplete: bool,
    /// Bytes written to the artifact.
    pub(crate) saved_bytes: usize,
}

/// Result of attempting to create an ephemeral artifact.
pub(crate) enum SavedArtifact {
    /// Artifact was written and is lifecycle-tracked.
    Available(SavedOutput),
    /// Platform or filesystem policy prevented private artifact creation.
    Unavailable,
}

/// Append standard saved-output metadata to a truncated tool result.
pub(crate) fn append_metadata(
    entries: &mut Vec<(tau_proto::CborValue, tau_proto::CborValue)>,
    rendered: &str,
) {
    use tau_proto::CborValue;
    entries.push((
        CborValue::Text("truncation_warning".to_owned()),
        CborValue::Text(
            "Fetching excessive output is inefficient; prefer narrower ranges or filters."
                .to_owned(),
        ),
    ));
    let mut end = rendered.len().min(MAX_SAVED_OUTPUT_BYTES);
    while !rendered.is_char_boundary(end) {
        end -= 1;
    }
    let incomplete = end < rendered.len();
    match save(&rendered[..end], incomplete) {
        Ok(saved) => {
            entries.push((
                CborValue::Text(
                    if saved.incomplete {
                        "saved_output_path"
                    } else {
                        "full_output_path"
                    }
                    .to_owned(),
                ),
                CborValue::Text(
                    saved
                        .path
                        .to_str()
                        .expect("spool accepts safe UTF-8 paths")
                        .to_owned(),
                ),
            ));
            if saved.incomplete {
                entries.push((
                    CborValue::Text("saved_output_truncated".to_owned()),
                    CborValue::Bool(true),
                ));
                entries.push((
                    CborValue::Text("saved_output_bytes".to_owned()),
                    CborValue::Integer((saved.saved_bytes as i64).into()),
                ));
            }
        }
        Err(_) => entries.push((
            CborValue::Text("saved_output_unavailable".to_owned()),
            CborValue::Bool(true),
        )),
    }
}

/// Advance cleanup accounting for one relevant ext-shell execution.
pub(crate) fn note_call() {
    tracker().note_call();
}

/// Save bounded rendered output in an unlistable private temporary directory.
pub(crate) fn save(output: &str, incomplete: bool) -> io::Result<SavedOutput> {
    save_parts(&[output], incomplete)
}

/// Save ordered native-rendering parts without assembling another large buffer.
pub(crate) fn save_parts(parts: &[&str], incomplete: bool) -> io::Result<SavedOutput> {
    let saved_bytes = parts
        .iter()
        .fold(0usize, |total, part| total.saturating_add(part.len()));
    if MAX_SAVED_OUTPUT_BYTES < saved_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "saved shell output exceeds hard cap",
        ));
    }
    let (directory, owner_lock) = create_private_directory()?;
    let path = directory.join(FILE_NAME);
    if path
        .to_str()
        .is_none_or(|path| path.chars().any(char::is_control))
    {
        let _ = remove_saved(&path);
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "temporary output path is not safe model metadata",
        ));
    }
    if let Err(error) = write_private_parts(&path, parts) {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(directory.join(LOCK_FILE_NAME));
        let _ = fs::remove_dir(&directory);
        return Err(error);
    }
    tracker().track(path.clone(), owner_lock);
    Ok(SavedOutput {
        path,
        incomplete,
        saved_bytes,
    })
}

fn write_private_parts(path: &Path, parts: &[&str]) -> io::Result<()> {
    let mut file = write_private_file_handle(path)?;
    for part in parts {
        file.write_all(part.as_bytes())?;
    }
    Ok(())
}

/// Remove all tracked output during graceful extension shutdown.
pub(crate) fn shutdown() {
    tracker().remove_all();
}

fn tracker() -> std::sync::MutexGuard<'static, Tracker> {
    TRACKER
        .get_or_init(|| Mutex::new(Tracker::default()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

impl Tracker {
    /// Advance call accounting and remove artifacts whose lifetime elapsed.
    fn note_call(&mut self) {
        self.call = self.call.saturating_add(1);
        if !self.scanned_leftovers {
            cleanup_crash_leftovers();
            self.scanned_leftovers = true;
        }
        let now = SystemTime::now();
        let call = self.call;
        self.files.retain(|saved| {
            let expired_by_age = now
                .duration_since(saved.created)
                .is_ok_and(|age| MAX_AGE <= age);
            let expired_by_calls = saved.call.saturating_add(MAX_LATER_CALLS) <= call;
            if expired_by_age && expired_by_calls {
                !remove_saved(&saved.path)
            } else {
                true
            }
        });
    }

    /// Add a newly written artifact to lifecycle tracking.
    fn track(&mut self, path: PathBuf, owner_lock: fs::File) {
        self.files.push(SavedFile {
            path,
            created: SystemTime::now(),
            call: self.call,
            _owner_lock: owner_lock,
        });
    }

    /// Remove every artifact still owned by this tracker.
    fn remove_all(&mut self) {
        self.files.retain(|saved| !remove_saved(&saved.path));
        for saved in &self.files {
            tracing::warn!(path = %saved.path.display(), "failed to remove ephemeral shell output");
        }
    }
}

fn cleanup_crash_leftovers() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(DIRECTORY_PREFIX)
        {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !metadata.file_type().is_dir() {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| MAX_AGE <= age);
        let lock_result = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(entry.path().join(LOCK_FILE_NAME));
        let owner_dead = match lock_result {
            Ok(file) => fs2::FileExt::try_lock_exclusive(&file).is_ok(),
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        };
        if old && owner_dead {
            let _ = remove_saved(&entry.path().join(FILE_NAME));
        }
    }
}

fn create_private_directory() -> io::Result<(PathBuf, fs::File)> {
    let directory = tempfile::Builder::new()
        .prefix(DIRECTORY_PREFIX)
        .tempdir_in(std::env::temp_dir())?;
    set_mode(directory.path(), 0o300)?;
    let owner_lock = write_private_file_handle(&directory.path().join(LOCK_FILE_NAME))?;
    fs2::FileExt::lock_exclusive(&owner_lock)?;
    let path = directory.keep();
    Ok((path, owner_lock))
}

#[cfg(test)]
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = write_private_file_handle(path)?;
    file.write_all(bytes)
}

fn write_private_file_handle(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn remove_saved(path: &Path) -> bool {
    let Some(directory) = path.parent() else {
        return false;
    };
    let output_removed = remove_if_present(path);
    let lock_removed = remove_if_present(&directory.join(LOCK_FILE_NAME));
    let directory_removed = match fs::remove_dir(directory) {
        Ok(()) => true,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    };
    output_removed && lock_removed && directory_removed
}

fn remove_if_present(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private shell output artifacts are unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests;

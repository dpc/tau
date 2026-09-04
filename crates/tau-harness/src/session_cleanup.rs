//! Best-effort cleanup of old per-session state directories.

use std::ffi as path_std_ffi;

#[cfg(test)]
mod tests;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt as _;
use tau_core::SessionMeta;
use tau_proto::SessionId;

static CLEANUP_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

struct SessionCandidate {
    /// Valid session identifier from the direct child name.
    session_id: SessionId,
    /// Parsed canonical manifest used for the first age filter.
    meta: SessionMeta,
    /// Identity of the direct child directory during enumeration.
    directory_metadata: fs::Metadata,
}

/// Aggregate content-free counters from one session cleanup pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SessionCleanupSummary {
    /// Canonical session manifests inspected.
    pub(crate) scanned: u64,
    /// Session directories atomically detached.
    pub(crate) detached: u64,
    /// Detached directories recursively removed.
    pub(crate) removed: u64,
    /// Candidate operations that failed.
    pub(crate) failures: u64,
}

#[cfg(test)]
pub(crate) fn cleanup_old_sessions(
    sessions_dir: PathBuf,
    retention: Duration,
    protected_sessions: Vec<SessionId>,
) {
    let cleanup_dir = detached_sessions_dir(&sessions_dir);
    if let Err(error) = remove_stale_detached_sessions(&sessions_dir) {
        tracing::warn!(
            target: "tau_harness::session_cleanup",
            cleanup_dir = %cleanup_dir.display(),
            %error,
            "failed to remove stale detached session directories"
        );
    }
    cleanup_old_sessions_with(
        sessions_dir,
        retention,
        protected_sessions,
        unix_now(),
        |path| fs::remove_dir_all(path),
    );
}

/// Removes expired sessions using one coordinator-owned wall-clock snapshot.
pub(crate) fn cleanup_old_sessions_at(
    sessions_dir: PathBuf,
    retention: Duration,
    protected_sessions: Vec<SessionId>,
    now: SystemTime,
) -> SessionCleanupSummary {
    let Ok(now) = now
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
    else {
        tracing::warn!(
            target: "tau_harness::retention_cleanup",
            "system clock before Unix epoch aborted session cleanup"
        );
        return SessionCleanupSummary {
            failures: 1,
            ..SessionCleanupSummary::default()
        };
    };
    cleanup_old_sessions_with(sessions_dir, retention, protected_sessions, now, |path| {
        fs::remove_dir_all(path)
    })
}

fn cleanup_old_sessions_with(
    sessions_dir: PathBuf,
    retention: Duration,
    protected_sessions: Vec<SessionId>,
    now: u64,
    remove_dir: impl FnMut(&std::path::Path) -> io::Result<()>,
) -> SessionCleanupSummary {
    cleanup_old_sessions_with_hooks(
        sessions_dir,
        retention,
        protected_sessions,
        now,
        remove_dir,
        |_| {},
        crate::retention_fs::sync_directory,
    )
}

pub(crate) fn cleanup_old_sessions_with_hooks(
    sessions_dir: PathBuf,
    retention: Duration,
    protected_sessions: Vec<SessionId>,
    now: u64,
    mut remove_dir: impl FnMut(&std::path::Path) -> io::Result<()>,
    mut before_lock: impl FnMut(&Path),
    mut sync_directory: impl FnMut(&Path) -> io::Result<()>,
) -> SessionCleanupSummary {
    let mut summary = SessionCleanupSummary::default();
    let candidates = match list_session_candidates(&sessions_dir) {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(
                target: "tau_harness::session_cleanup",
                sessions_dir = %sessions_dir.display(),
                %error,
                "failed to list session metadata for cleanup"
            );
            summary.failures += 1;
            return summary;
        }
    };
    summary.scanned = candidates.len() as u64;

    for candidate in candidates {
        let session_id = candidate.session_id;
        let meta = candidate.meta;
        if protected_sessions.contains(&session_id) {
            continue;
        }
        if !expired_unix_seconds(now, meta.last_touched, retention) {
            continue;
        }

        let path = sessions_dir.join(session_id.as_str());
        if !path_still_names_directory(&path, &candidate.directory_metadata) {
            continue;
        }
        before_lock(&path);
        let cleanup_lock = match try_acquire_existing_cleanup_lock(&path.join("lock")) {
            Ok(Some(lock)) => lock,
            Ok(None) => continue,
            Err(error) => {
                summary.failures += 1;
                tracing::warn!(
                    target: "tau_harness::session_cleanup",
                    session_id = %session_id,
                    %error,
                    "failed to acquire session lock for cleanup"
                );
                continue;
            }
        };
        if !path_still_names_directory(&path, &candidate.directory_metadata) {
            continue;
        }
        let meta_path = path.join("meta.json");
        let (_current_meta, manifest_metadata) = match read_session_meta_nofollow(&meta_path) {
            Ok(current) if !expired_unix_seconds(now, current.0.last_touched, retention) => {
                continue;
            }
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                summary.failures += 1;
                tracing::warn!(
                    target: "tau_harness::session_cleanup",
                    session_id = %session_id,
                    %error,
                    "failed to revalidate session metadata for cleanup"
                );
                continue;
            }
        };
        if !path_still_names_directory(&path, &candidate.directory_metadata)
            || !path_still_names_file(&meta_path, &manifest_metadata)
        {
            continue;
        }

        let (detached_path, durability) =
            match detach_session_dir(&sessions_dir, &path, &session_id, &mut sync_directory) {
                Ok(detached) => detached,
                Err(error) => {
                    summary.failures += 1;
                    tracing::warn!(
                        target: "tau_harness::session_cleanup",
                        session_id = %session_id,
                        path = %path.display(),
                        %error,
                        "failed to detach old session directory"
                    );
                    continue;
                }
            };
        summary.detached += 1;
        if let Err(error) = durability {
            summary.failures += 1;
            tracing::warn!(
                target: "tau_harness::session_cleanup",
                session_id = %session_id,
                path = %detached_path.display(),
                %error,
                "failed to commit detached session staging boundary"
            );
            drop(cleanup_lock);
            continue;
        }
        let result = crate::retention_fs::remove_staged_tree(
            &detached_path,
            &detached_sessions_dir(&sessions_dir),
            &mut remove_dir,
            &mut sync_directory,
        );
        drop(cleanup_lock);
        if let Err(error) = result {
            summary.failures += 1;
            tracing::warn!(
                target: "tau_harness::session_cleanup",
                session_id = %session_id,
                path = %detached_path.display(),
                %error,
                "failed to remove detached session directory"
            );
        } else {
            summary.removed += 1;
        }
    }
    summary
}

fn expired_unix_seconds(now: u64, timestamp: u64, retention: Duration) -> bool {
    now.checked_sub(timestamp)
        .is_some_and(|age| retention.as_secs() <= age)
}

fn list_session_candidates(sessions_dir: &Path) -> io::Result<Vec<SessionCandidate>> {
    let mut candidates = Vec::new();
    let entries = match fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(candidates),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        let Ok(session_id) = SessionId::parse(name) else {
            continue;
        };
        let path = entry.path();
        let Ok((meta, _)) = read_session_meta_nofollow(&path.join("meta.json")) else {
            continue;
        };
        candidates.push(SessionCandidate {
            session_id,
            meta,
            directory_metadata: entry.metadata()?,
        });
    }
    Ok(candidates)
}

fn read_session_meta_nofollow(path: &Path) -> io::Result<(SessionMeta, fs::Metadata)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session manifest is not a regular file",
        ));
    }
    let mut bytes = String::new();
    file.read_to_string(&mut bytes)?;
    let meta = serde_json::from_str(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((meta, metadata))
}

fn path_still_names_directory(path: &Path, expected: &fs::Metadata) -> bool {
    path_still_names(path, expected, true)
}

fn path_still_names_file(path: &Path, expected: &fs::Metadata) -> bool {
    path_still_names(path, expected, false)
}

fn path_still_names(path: &Path, expected: &fs::Metadata, directory: bool) -> bool {
    let Ok(current) = fs::symlink_metadata(path) else {
        return false;
    };
    if current.file_type().is_symlink()
        || (directory && !current.is_dir())
        || (!directory && !current.is_file())
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        expected.dev() == current.dev() && expected.ino() == current.ino()
    }
    #[cfg(not(unix))]
    {
        expected.modified().ok() == current.modified().ok()
    }
}

fn try_acquire_existing_cleanup_lock(path: &Path) -> io::Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session lock is not a regular file",
        ));
    }
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

fn detach_session_dir(
    sessions_dir: &Path,
    session_path: &Path,
    session_id: &SessionId,
    sync_directory: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<(PathBuf, io::Result<()>)> {
    let cleanup_dir = detached_sessions_dir(sessions_dir);
    crate::retention_fs::prepare_staging_directory(&cleanup_dir, sync_directory)?;
    loop {
        let suffix = CLEANUP_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let detached_path = cleanup_dir.join(format!(
            "{}-{}-{suffix}",
            session_id.as_str(),
            std::process::id()
        ));
        match fs::rename(session_path, &detached_path) {
            Ok(()) => {
                let durability = crate::retention_fs::sync_detach_boundary(
                    sessions_dir,
                    &cleanup_dir,
                    sync_directory,
                );
                return Ok((detached_path, durability));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn detached_sessions_dir(sessions_dir: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(
        sessions_dir
            .file_name()
            .unwrap_or_else(|| path_std_ffi::OsStr::new("sessions")),
    );
    name.push(".cleanup");
    sessions_dir.with_file_name(name)
}

/// Finalizes session deletions committed by an earlier successful detach.
pub(crate) fn finalize_detached_sessions(sessions_dir: &Path) -> io::Result<()> {
    remove_stale_detached_sessions(sessions_dir)
}

fn remove_stale_detached_sessions(sessions_dir: &Path) -> io::Result<()> {
    remove_stale_detached_sessions_with(
        sessions_dir,
        |path| fs::remove_dir_all(path),
        crate::retention_fs::sync_directory,
    )
}

fn remove_stale_detached_sessions_with(
    sessions_dir: &Path,
    mut remove_dir: impl FnMut(&Path) -> io::Result<()>,
    mut sync_directory: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let cleanup_dir = detached_sessions_dir(sessions_dir);
    let entries = match fs::read_dir(&cleanup_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        crate::retention_fs::sync_detach_boundary(sessions_dir, &cleanup_dir, &mut sync_directory)?;
        match crate::retention_fs::remove_staged_tree(
            &path,
            &cleanup_dir,
            &mut remove_dir,
            &mut sync_directory,
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Acquire one session's cleanup lock without waiting.
///
/// `None` means another process currently holds the lock.
pub(crate) fn try_acquire_cleanup_lock(path: &std::path::Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

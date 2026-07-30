//! Best-effort cleanup of old per-session state directories.

use std::{ffi as path_std_ffi, thread as path_std_thread};

#[cfg(test)]
mod tests;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt as _;
use tau_core::{SessionMeta, list_session_metas};
use tau_proto::SessionId;

static CLEANUP_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn spawn_session_cleanup(
    sessions_dir: PathBuf,
    retention: Option<Duration>,
    protected_sessions: Vec<SessionId>,
) {
    let Some(retention) = retention else {
        return;
    };

    if let Err(error) = path_std_thread::Builder::new()
        .name("tau-session-cleanup".to_owned())
        .spawn(move || cleanup_old_sessions(sessions_dir, retention, protected_sessions))
    {
        tracing::warn!(
            target: "tau_harness::session_cleanup",
            %error,
            "failed to spawn session cleanup thread"
        );
    }
}

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
    cleanup_old_sessions_with(sessions_dir, retention, protected_sessions, |path| {
        fs::remove_dir_all(path)
    });
}

fn cleanup_old_sessions_with(
    sessions_dir: PathBuf,
    retention: Duration,
    protected_sessions: Vec<SessionId>,
    mut remove_dir: impl FnMut(&std::path::Path) -> io::Result<()>,
) {
    let cutoff = unix_now().saturating_sub(retention.as_secs());
    let metas = match list_session_metas(&sessions_dir) {
        Ok(metas) => metas,
        Err(error) => {
            tracing::warn!(
                target: "tau_harness::session_cleanup",
                sessions_dir = %sessions_dir.display(),
                %error,
                "failed to list session metadata for cleanup"
            );
            return;
        }
    };

    for (session_id, meta) in metas {
        if protected_sessions.contains(&session_id) {
            continue;
        }
        if cutoff < meta.last_touched {
            continue;
        }

        let path = sessions_dir.join(session_id.as_str());
        let cleanup_lock = match try_acquire_cleanup_lock(&path.join("lock")) {
            Ok(Some(lock)) => lock,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness::session_cleanup",
                    session_id = %session_id,
                    %error,
                    "failed to acquire session lock for cleanup"
                );
                continue;
            }
        };
        match read_session_meta(&path.join("meta.json")) {
            Ok(current_meta) if cutoff < current_meta.last_touched => continue,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness::session_cleanup",
                    session_id = %session_id,
                    %error,
                    "failed to revalidate session metadata for cleanup"
                );
                continue;
            }
        }

        let detached_path = match detach_session_dir(&sessions_dir, &path, &session_id) {
            Ok(path) => path,
            Err(error) => {
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
        let result = remove_dir(&detached_path);
        drop(cleanup_lock);
        if let Err(error) = result {
            tracing::warn!(
                target: "tau_harness::session_cleanup",
                session_id = %session_id,
                path = %detached_path.display(),
                %error,
                "failed to remove detached session directory"
            );
        }
    }
}

fn read_session_meta(path: &Path) -> io::Result<SessionMeta> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn detach_session_dir(
    sessions_dir: &Path,
    session_path: &Path,
    session_id: &SessionId,
) -> io::Result<PathBuf> {
    let cleanup_dir = detached_sessions_dir(sessions_dir);
    fs::create_dir_all(&cleanup_dir)?;
    loop {
        let suffix = CLEANUP_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let detached_path = cleanup_dir.join(format!(
            "{}-{}-{suffix}",
            session_id.as_str(),
            std::process::id()
        ));
        match fs::rename(session_path, &detached_path) {
            Ok(()) => return Ok(detached_path),
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

fn remove_stale_detached_sessions(sessions_dir: &Path) -> io::Result<()> {
    let cleanup_dir = detached_sessions_dir(sessions_dir);
    let entries = match fs::read_dir(&cleanup_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        match fs::remove_dir_all(&path) {
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

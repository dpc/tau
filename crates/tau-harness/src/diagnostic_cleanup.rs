//! Best-effort startup cleanup for non-authoritative session diagnostics.

use tau_config::provider_debug_capture as path_tau_config_provider_debug_capture;

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::thread;
use std::time::{Duration, SystemTime};
use std::{fs, io};

/// Aggregate content-free counters from one diagnostic cleanup pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DiagnosticCleanupSummary {
    /// Recognized regular diagnostic files inspected.
    pub(crate) scanned: u64,
    /// Expired diagnostic files removed.
    pub(crate) removed: u64,
    /// Inspection or removal operations that failed.
    pub(crate) failures: u64,
}

/// Spawn one opportunistic cleanup pass for session debug files.
/// Start cleanup for tests that need to observe thread completion.
#[cfg(test)]
fn spawn_diagnostic_cleanup_for_test(
    sessions_dir: PathBuf,
    retention: Option<Duration>,
    persistence: tau_core::SessionPersistenceMode,
    protected_sessions: Vec<tau_proto::SessionId>,
) -> io::Result<Option<std::thread::JoinHandle<()>>> {
    spawn_diagnostic_cleanup_inner(sessions_dir, retention, persistence, protected_sessions)
}

#[cfg(test)]
fn spawn_diagnostic_cleanup_inner(
    sessions_dir: PathBuf,
    retention: Option<Duration>,
    persistence: tau_core::SessionPersistenceMode,
    protected_sessions: Vec<tau_proto::SessionId>,
) -> io::Result<Option<std::thread::JoinHandle<()>>> {
    let Some(retention) = retention.filter(|_| persistence.is_durable()) else {
        return Ok(None);
    };
    thread::Builder::new()
        .name("tau-diagnostic-cleanup".to_owned())
        .spawn(move || cleanup_diagnostics(&sessions_dir, retention, &protected_sessions))
        .map(Some)
}

/// Remove expired JSONL mirrors and provider request/response captures.
#[cfg(test)]
fn cleanup_diagnostics(
    sessions_dir: &Path,
    retention: Duration,
    protected_sessions: &[tau_proto::SessionId],
) {
    cleanup_diagnostics_with(
        sessions_dir,
        retention,
        SystemTime::now(),
        protected_sessions,
        |path| fs::remove_file(path),
    );
}

/// Removes expired diagnostics using one coordinator-owned wall-clock snapshot.
pub(crate) fn cleanup_diagnostics_at(
    sessions_dir: &Path,
    retention: Duration,
    now: SystemTime,
    protected_sessions: &[tau_proto::SessionId],
) -> DiagnosticCleanupSummary {
    cleanup_diagnostics_with(sessions_dir, retention, now, protected_sessions, |path| {
        fs::remove_file(path)
    })
}

/// Run cleanup with injectable time and removal for deterministic fault tests.
fn cleanup_diagnostics_with(
    sessions_dir: &Path,
    retention: Duration,
    now: SystemTime,
    protected_sessions: &[tau_proto::SessionId],
    mut remove_file: impl FnMut(&Path) -> io::Result<()>,
) -> DiagnosticCleanupSummary {
    cleanup_diagnostics_with_lock(
        sessions_dir,
        retention,
        now,
        protected_sessions,
        &mut remove_file,
        try_acquire_diagnostic_cleanup_lock,
    )
}

/// Run cleanup with injectable time, removal, and lock acquisition for
/// deterministic fault tests.
fn cleanup_diagnostics_with_lock(
    sessions_dir: &Path,
    retention: Duration,
    now: SystemTime,
    protected_sessions: &[tau_proto::SessionId],
    remove_file: &mut impl FnMut(&Path) -> io::Result<()>,
    mut acquire_lock: impl FnMut(&Path) -> io::Result<Option<std::fs::File>>,
) -> DiagnosticCleanupSummary {
    let mut summary = DiagnosticCleanupSummary::default();
    let entries = match fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return summary,
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(
                target: "tau_harness::diagnostic_cleanup",
                sessions_dir = %sessions_dir.display(),
                %error,
                "failed to list session diagnostics for cleanup"
            );
            return summary;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                summary.failures += 1;
                tracing::warn!(
                    target: "tau_harness::diagnostic_cleanup",
                    %error,
                    "failed to inspect one session directory during diagnostic cleanup"
                );
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                summary.failures += 1;
                tracing::warn!(
                    target: "tau_harness::diagnostic_cleanup",
                    path = %entry.path().display(),
                    %error,
                    "failed to inspect session path during diagnostic cleanup"
                );
                continue;
            }
        };
        if !file_type.is_dir() {
            continue;
        }
        if protected_sessions
            .iter()
            .any(|session_id| entry.file_name() == session_id.as_str())
        {
            continue;
        }
        let _session_lock = match acquire_lock(&entry.path()) {
            Ok(Some(lock)) => lock,
            Ok(None) => continue,
            Err(error) => {
                summary.failures += 1;
                tracing::warn!(
                    target: "tau_harness::diagnostic_cleanup",
                    path = %entry.path().display(),
                    %error,
                    "failed to acquire session lock for diagnostic cleanup"
                );
                continue;
            }
        };
        cleanup_candidate(
            &entry.path().join("events.jsonl"),
            retention,
            now,
            remove_file,
            &mut summary,
        );
        cleanup_provider_captures(&entry.path(), retention, now, remove_file, &mut summary);
    }
    summary
}

/// Acquire a cleanup lock or silently skip a session that vanished after
/// enumeration.
fn try_acquire_diagnostic_cleanup_lock(session_dir: &Path) -> io::Result<Option<std::fs::File>> {
    match crate::session_cleanup::try_acquire_cleanup_lock(&session_dir.join("lock")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fs::symlink_metadata(session_dir) {
                Err(recheck_error) if recheck_error.kind() == io::ErrorKind::NotFound => Ok(None),
                _ => Err(error),
            }
        }
        result => result,
    }
}

/// Remove recognized captures from one real provider-capture directory.
fn cleanup_provider_captures(
    session_dir: &Path,
    retention: Duration,
    now: SystemTime,
    remove_file: &mut impl FnMut(&Path) -> io::Result<()>,
    summary: &mut DiagnosticCleanupSummary,
) {
    let debug_dir = session_dir.join("debug");
    let capture_dir = debug_dir.join("provider-requests");
    if !is_real_directory(&debug_dir) || !is_real_directory(&capture_dir) {
        return;
    }
    let entries = match fs::read_dir(&capture_dir) {
        Ok(entries) => entries,
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(
                target: "tau_harness::diagnostic_cleanup",
                path = %capture_dir.display(),
                %error,
                "failed to list provider debug captures for cleanup"
            );
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                summary.failures += 1;
                tracing::warn!(
                    target: "tau_harness::diagnostic_cleanup",
                    path = %capture_dir.display(),
                    %error,
                    "failed to inspect one provider debug capture"
                );
                continue;
            }
        };
        let path = entry.path();
        if is_real_directory(&path) {
            cleanup_provider_capture_directory(&path, retention, now, remove_file, summary);
        } else {
            cleanup_provider_capture_candidate(&path, retention, now, remove_file, summary);
        }
    }
}

/// Remove recognized capture files from one provider capture sink.
fn cleanup_provider_capture_directory(
    directory: &Path,
    retention: Duration,
    now: SystemTime,
    remove_file: &mut impl FnMut(&Path) -> io::Result<()>,
    summary: &mut DiagnosticCleanupSummary,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        cleanup_provider_capture_candidate(&entry.path(), retention, now, remove_file, summary);
    }
}

/// Remove one recognized provider capture file, retaining unrelated entries.
fn cleanup_provider_capture_candidate(
    path: &Path,
    retention: Duration,
    now: SystemTime,
    remove_file: &mut impl FnMut(&Path) -> io::Result<()>,
    summary: &mut DiagnosticCleanupSummary,
) {
    let Some(filename) = path.file_name().and_then(OsStr::to_str) else {
        return;
    };
    if is_provider_capture_filename(filename) {
        cleanup_candidate(path, retention, now, remove_file, summary);
    }
}

/// Return whether a filename is one compressed provider capture.
fn is_provider_capture_filename(filename: &str) -> bool {
    path_tau_config_provider_debug_capture::ProviderDebugCaptureFilename::parse(filename).is_some()
}

/// Return whether `path` is a directory and not a final-component symlink.
fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// Remove one expired regular diagnostic file without following symlinks.
fn cleanup_candidate(
    path: &Path,
    retention: Duration,
    now: SystemTime,
    remove_file: &mut impl FnMut(&Path) -> io::Result<()>,
    summary: &mut DiagnosticCleanupSummary,
) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(
                target: "tau_harness::diagnostic_cleanup",
                path = %path.display(),
                %error,
                "failed to inspect diagnostic file"
            );
            return;
        }
    };
    summary.scanned += 1;
    let expired = metadata
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| retention <= age);
    if !expired {
        return;
    }
    match remove_file(path) {
        Ok(()) => summary.removed += 1,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            summary.failures += 1;
            tracing::warn!(
                target: "tau_harness::diagnostic_cleanup",
                path = %path.display(),
                %error,
                "failed to remove expired diagnostic file"
            );
        }
    }
}

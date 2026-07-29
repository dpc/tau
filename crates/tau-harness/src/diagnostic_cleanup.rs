//! Best-effort startup cleanup for non-authoritative JSONL diagnostics.

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use std::{fs, io};

/// Spawn one opportunistic cleanup pass for session debug JSONL files.
pub(crate) fn spawn_diagnostic_jsonl_cleanup(
    sessions_dir: PathBuf,
    retention: Option<Duration>,
    persistence: tau_core::SessionPersistenceMode,
    protected_sessions: Vec<tau_proto::SessionId>,
) {
    if let Err(error) = spawn_diagnostic_jsonl_cleanup_inner(
        sessions_dir,
        retention,
        persistence,
        protected_sessions,
    ) {
        tracing::warn!(
            target: "tau_harness::diagnostic_cleanup",
            %error,
            "failed to spawn diagnostic JSONL cleanup thread"
        );
    }
}

/// Start cleanup for tests that need to observe thread completion.
#[cfg(test)]
fn spawn_diagnostic_jsonl_cleanup_for_test(
    sessions_dir: PathBuf,
    retention: Option<Duration>,
    persistence: tau_core::SessionPersistenceMode,
    protected_sessions: Vec<tau_proto::SessionId>,
) -> io::Result<Option<std::thread::JoinHandle<()>>> {
    spawn_diagnostic_jsonl_cleanup_inner(sessions_dir, retention, persistence, protected_sessions)
}

fn spawn_diagnostic_jsonl_cleanup_inner(
    sessions_dir: PathBuf,
    retention: Option<Duration>,
    persistence: tau_core::SessionPersistenceMode,
    protected_sessions: Vec<tau_proto::SessionId>,
) -> io::Result<Option<std::thread::JoinHandle<()>>> {
    let Some(retention) = retention.filter(|_| persistence.is_durable()) else {
        return Ok(None);
    };
    std::thread::Builder::new()
        .name("tau-diagnostic-cleanup".to_owned())
        .spawn(move || cleanup_diagnostic_jsonl(&sessions_dir, retention, &protected_sessions))
        .map(Some)
}

/// Remove expired `events.jsonl` files directly below session directories.
fn cleanup_diagnostic_jsonl(
    sessions_dir: &Path,
    retention: Duration,
    protected_sessions: &[tau_proto::SessionId],
) {
    cleanup_diagnostic_jsonl_with(
        sessions_dir,
        retention,
        SystemTime::now(),
        protected_sessions,
        |path| fs::remove_file(path),
    );
}

/// Run cleanup with injectable time and removal for deterministic fault tests.
fn cleanup_diagnostic_jsonl_with(
    sessions_dir: &Path,
    retention: Duration,
    now: SystemTime,
    protected_sessions: &[tau_proto::SessionId],
    mut remove_file: impl FnMut(&Path) -> io::Result<()>,
) {
    let entries = match fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(
                target: "tau_harness::diagnostic_cleanup",
                sessions_dir = %sessions_dir.display(),
                %error,
                "failed to list session diagnostics for cleanup"
            );
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
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
        let _session_lock =
            match crate::session_cleanup::try_acquire_cleanup_lock(&entry.path().join("lock")) {
                Ok(Some(lock)) => lock,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        target: "tau_harness::diagnostic_cleanup",
                        path = %entry.path().display(),
                        %error,
                        "failed to acquire session lock for diagnostic cleanup"
                    );
                    continue;
                }
            };
        let path = entry.path().join("events.jsonl");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness::diagnostic_cleanup",
                    path = %path.display(),
                    %error,
                    "failed to inspect diagnostic JSONL file"
                );
                continue;
            }
        };
        let expired = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| retention <= age);
        if !expired {
            continue;
        }
        if let Err(error) = remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                target: "tau_harness::diagnostic_cleanup",
                path = %path.display(),
                %error,
                "failed to remove expired diagnostic JSONL file"
            );
        }
    }
}

//! Best-effort cleanup of old per-session state directories.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tau_core::{list_session_metas, session_is_locked};
use tau_proto::SessionId;

pub(crate) fn spawn_session_cleanup(
    sessions_dir: PathBuf,
    retention: Option<Duration>,
    protected_sessions: Vec<SessionId>,
) {
    let Some(retention) = retention else {
        return;
    };

    if let Err(error) = std::thread::Builder::new()
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
        match session_is_locked(&sessions_dir, session_id.as_str()) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness::session_cleanup",
                    session_id = %session_id,
                    %error,
                    "failed to inspect session lock for cleanup"
                );
                continue;
            }
        }
        if cutoff < meta.last_touched {
            continue;
        }

        let path = sessions_dir.join(session_id.as_str());
        if let Err(error) = fs::remove_dir_all(&path) {
            tracing::warn!(
                target: "tau_harness::session_cleanup",
                session_id = %session_id,
                path = %path.display(),
                %error,
                "failed to remove old session directory"
            );
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::time::Duration;

    use fs2::FileExt as _;
    use tau_core::SessionMeta;
    use tau_proto::SessionId;
    use tempfile::TempDir;

    use super::cleanup_old_sessions;

    fn write_session_meta(root: &std::path::Path, session_id: &str, last_touched: u64) {
        let dir = root.join(session_id);
        std::fs::create_dir_all(&dir).expect("session dir");
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_vec(&SessionMeta {
                created_at: last_touched,
                last_touched,
            })
            .expect("meta json"),
        )
        .expect("write meta");
    }

    /// Ensures cleanup does not remove the active session even when its
    /// metadata is older than the retention window.
    #[test]
    fn cleanup_skips_protected_current_session() {
        let temp = TempDir::new().expect("temp sessions");
        write_session_meta(temp.path(), "current", 0);

        cleanup_old_sessions(
            temp.path().to_path_buf(),
            Duration::from_secs(1),
            vec![SessionId::from("current")],
        );

        assert!(temp.path().join("current").exists());
    }

    /// Ensures cleanup does not remove a session that another harness process
    /// currently has locked.
    #[test]
    fn cleanup_skips_locked_session() {
        let temp = TempDir::new().expect("temp sessions");
        write_session_meta(temp.path(), "locked", 0);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(temp.path().join("locked").join("lock"))
            .expect("lock file");
        lock.try_lock_exclusive().expect("hold lock");

        cleanup_old_sessions(
            temp.path().to_path_buf(),
            Duration::from_secs(1),
            Vec::new(),
        );

        assert!(temp.path().join("locked").exists());
        lock.unlock().expect("unlock");
    }

    /// Ensures old unlocked sessions remain eligible for retention cleanup.
    #[test]
    fn cleanup_removes_old_unlocked_session() {
        let temp = TempDir::new().expect("temp sessions");
        write_session_meta(temp.path(), "old", 0);

        cleanup_old_sessions(
            temp.path().to_path_buf(),
            Duration::from_secs(1),
            Vec::new(),
        );

        assert!(!temp.path().join("old").exists());
    }
}

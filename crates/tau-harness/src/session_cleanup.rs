//! Best-effort cleanup of old per-session state directories.

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
            .unwrap_or_else(|| std::ffi::OsStr::new("sessions")),
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

fn try_acquire_cleanup_lock(path: &std::path::Path) -> io::Result<Option<File>> {
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

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::time::Duration;

    use fs2::FileExt as _;
    use tau_core::{SessionMeta, SessionStore};
    use tau_proto::{AgentId, Event, SessionAgentLoaded, SessionId};
    use tempfile::TempDir;

    use super::{cleanup_old_sessions, cleanup_old_sessions_with, detached_sessions_dir};

    fn sessions_dir(temp: &TempDir) -> std::path::PathBuf {
        temp.path().join("sessions")
    }

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
        let sessions_dir = sessions_dir(&temp);
        write_session_meta(&sessions_dir, "current", 0);

        cleanup_old_sessions(
            sessions_dir.clone(),
            Duration::from_secs(1),
            vec![SessionId::from("current")],
        );

        assert!(sessions_dir.join("current").exists());
    }

    /// Ensures cleanup does not remove a session that another harness process
    /// currently has locked.
    #[test]
    fn cleanup_skips_locked_session() {
        let temp = TempDir::new().expect("temp sessions");
        let sessions_dir = sessions_dir(&temp);
        write_session_meta(&sessions_dir, "locked", 0);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(sessions_dir.join("locked").join("lock"))
            .expect("lock file");
        lock.try_lock_exclusive().expect("hold lock");

        cleanup_old_sessions(sessions_dir.clone(), Duration::from_secs(1), Vec::new());

        assert!(sessions_dir.join("locked").exists());
        lock.unlock().expect("unlock");
    }

    /// Ensures cleanup atomically detaches an expired tree before removing it,
    /// while a writer that preloaded the old cursor safely reloads the
    /// recreated path and starts its replacement journal at sequence zero.
    #[test]
    fn cleanup_detaches_tree_before_removal_and_writer_reload() {
        let temp = TempDir::new().expect("temp sessions");
        let sessions_dir = sessions_dir(&temp);
        let mut setup = SessionStore::open(&sessions_dir).expect("setup session store");
        setup
            .append_session_event(
                "old",
                None,
                Event::SessionAgentLoaded(SessionAgentLoaded {
                    session_id: SessionId::from("old"),
                    agent_id: AgentId::parse("old-agent").expect("agent id"),
                    ephemeral: false,
                }),
            )
            .expect("old membership append");
        drop(setup);
        write_session_meta(&sessions_dir, "old", 0);
        let mut writer = SessionStore::open(&sessions_dir).expect("preloaded competing store");
        let mut removal_observed = false;

        cleanup_old_sessions_with(
            sessions_dir.clone(),
            Duration::from_secs(1),
            Vec::new(),
            |detached_path| {
                removal_observed = true;
                assert_eq!(
                    detached_path.parent(),
                    Some(detached_sessions_dir(&sessions_dir).as_path()),
                    "cleanup must remove a detached tree"
                );
                assert!(
                    !sessions_dir.join("old").exists(),
                    "the original path must be detached before removal starts"
                );
                let membership = writer
                    .lock_and_load_session("old")
                    .expect("writer acquires recreated path");
                assert!(
                    membership.is_none(),
                    "recreated path must not synthesize a durable session"
                );
                let outcome = writer
                    .append_session_event(
                        "old",
                        None,
                        Event::SessionAgentLoaded(SessionAgentLoaded {
                            session_id: SessionId::from("old"),
                            agent_id: AgentId::parse("new-agent").expect("agent id"),
                            ephemeral: false,
                        }),
                    )
                    .expect("writer appends to recreated journal");
                assert_eq!(
                    outcome.seq.get(),
                    0,
                    "recreated journal must start at sequence zero"
                );
                Ok(())
            },
        );

        assert!(removal_observed, "expired session must reach removal");
        assert!(
            sessions_dir.join("old").join("events.cbor").exists(),
            "cleanup must not remove the writer's recreated journal"
        );
    }

    /// Ensures a cleanup run removes detached trees left by an interrupted
    /// prior run without exposing them as session directories.
    #[test]
    fn cleanup_removes_stale_detached_trees() {
        let temp = TempDir::new().expect("temp sessions");
        let sessions_dir = sessions_dir(&temp);
        let stale = detached_sessions_dir(&sessions_dir).join("old-123-0");
        std::fs::create_dir_all(&stale).expect("stale detached tree");
        std::fs::write(stale.join("events.cbor"), b"stale").expect("stale journal");

        cleanup_old_sessions(sessions_dir, Duration::from_secs(1), Vec::new());

        assert!(!stale.exists(), "stale detached tree must be removed");
    }

    /// Ensures the cleanup staging area remains outside the session-ID
    /// namespace, where `.cleanup` is a valid live session name.
    #[test]
    fn cleanup_staging_does_not_collide_with_dot_cleanup_session() {
        let temp = TempDir::new().expect("temp sessions");
        let sessions_dir = sessions_dir(&temp);
        write_session_meta(&sessions_dir, ".cleanup", u64::MAX);
        write_session_meta(&sessions_dir, "old", 0);

        cleanup_old_sessions(sessions_dir.clone(), Duration::from_secs(1), Vec::new());

        assert!(
            sessions_dir.join(".cleanup").join("meta.json").exists(),
            "valid .cleanup session must remain untouched"
        );
        assert!(
            !sessions_dir.join("old").exists(),
            "expired ordinary session must still be cleaned"
        );
    }

    /// Ensures old unlocked sessions remain eligible for retention cleanup.
    #[test]
    fn cleanup_removes_old_unlocked_session() {
        let temp = TempDir::new().expect("temp sessions");
        let sessions_dir = sessions_dir(&temp);
        write_session_meta(&sessions_dir, "old", 0);

        cleanup_old_sessions(sessions_dir.clone(), Duration::from_secs(1), Vec::new());

        assert!(!sessions_dir.join("old").exists());
    }
}

use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
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
    std::fs::write(dir.join("lock"), []).expect("write lock");
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
        vec![SessionId::parse("current").expect("known-safe SessionId must be valid")],
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
                agent_initialization_id: tau_proto::AgentInitializationId::parse("test-init")
                    .expect("test identifier must be valid"),

                session_id: SessionId::parse("old").expect("known-safe SessionId must be valid"),
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
        u64::MAX,
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
                        agent_initialization_id: tau_proto::AgentInitializationId::parse(
                            "test-init",
                        )
                        .expect("test identifier must be valid"),

                        session_id: SessionId::parse("old")
                            .expect("known-safe SessionId must be valid"),
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

/// Retention includes the exact cutoff and preserves a manifest touched one
/// second later, preventing off-by-one deletion at the configured boundary.
#[test]
fn cleanup_applies_exact_retention_boundary() {
    let temp = TempDir::new().expect("temp sessions");
    let sessions_dir = sessions_dir(&temp);
    write_session_meta(&sessions_dir, "at-cutoff", 90);
    write_session_meta(&sessions_dir, "after-cutoff", 91);

    cleanup_old_sessions_with(
        sessions_dir.clone(),
        Duration::from_secs(10),
        Vec::new(),
        100,
        |path| std::fs::remove_dir_all(path),
    );

    assert!(!sessions_dir.join("at-cutoff").exists());
    assert!(sessions_dir.join("after-cutoff").exists());
}

/// Cleanup fails safe when canonical existence cannot be validated, preserving
/// both missing-manifest and malformed-manifest session directories.
#[test]
fn cleanup_preserves_sessions_with_invalid_manifests() {
    let temp = TempDir::new().expect("temp sessions");
    let sessions_dir = sessions_dir(&temp);
    let missing = sessions_dir.join("missing");
    let malformed = sessions_dir.join("malformed");
    std::fs::create_dir_all(&missing).expect("create missing-manifest session");
    std::fs::create_dir_all(&malformed).expect("create malformed-manifest session");
    std::fs::write(malformed.join("meta.json"), b"{not-json").expect("write malformed manifest");

    cleanup_old_sessions_with(
        sessions_dir.clone(),
        Duration::from_secs(10),
        Vec::new(),
        100,
        |path| std::fs::remove_dir_all(path),
    );

    assert!(missing.exists());
    assert!(malformed.exists());
}

/// Session discovery never follows a namespace symlink or creates a lock
/// through it while deciding retention eligibility.
#[cfg(unix)]
#[test]
fn cleanup_ignores_symlinked_session_entry_without_touching_target() {
    let temp = TempDir::new().expect("temp sessions");
    let sessions_dir = sessions_dir(&temp);
    let external = temp.path().join("external");
    write_session_meta(&external, "target", 0);
    std::fs::remove_file(external.join("target/lock")).expect("remove seeded target lock");
    std::fs::create_dir_all(&sessions_dir).expect("sessions root");
    unix_fs::symlink(external.join("target"), sessions_dir.join("linked"))
        .expect("session symlink");

    cleanup_old_sessions_with(
        sessions_dir.clone(),
        Duration::from_secs(1),
        Vec::new(),
        100,
        |path| std::fs::remove_dir_all(path),
    );

    assert!(sessions_dir.join("linked").is_symlink());
    assert!(external.join("target/meta.json").exists());
    assert!(!external.join("target/lock").exists());
}

/// A direct-child replacement after enumeration cannot inherit the old
/// candidate's eligibility or be detached under the old identity.
#[test]
fn cleanup_revalidates_session_directory_identity_after_entry_swap() {
    let temp = TempDir::new().expect("temp sessions");
    let sessions_dir = sessions_dir(&temp);
    write_session_meta(&sessions_dir, "swapped", 0);
    let displaced = temp.path().join("displaced");
    let mut swapped = false;

    super::cleanup_old_sessions_with_hooks(
        sessions_dir.clone(),
        Duration::from_secs(1),
        Vec::new(),
        100,
        |path| std::fs::remove_dir_all(path),
        |path| {
            if !swapped {
                swapped = true;
                std::fs::rename(path, &displaced).expect("displace enumerated tree");
                write_session_meta(&sessions_dir, "swapped", 0);
            }
        },
    );

    assert!(sessions_dir.join("swapped/meta.json").exists());
    assert!(displaced.join("meta.json").exists());
}

/// A non-regular lock object cannot authorize session deletion.
#[test]
fn cleanup_preserves_session_with_nonregular_lock() {
    let temp = TempDir::new().expect("temp sessions");
    let sessions_dir = sessions_dir(&temp);
    write_session_meta(&sessions_dir, "bad-lock", 0);
    std::fs::remove_file(sessions_dir.join("bad-lock/lock")).expect("remove lock file");
    std::fs::create_dir(sessions_dir.join("bad-lock/lock")).expect("directory lock artifact");

    cleanup_old_sessions_with(
        sessions_dir.clone(),
        Duration::from_secs(1),
        Vec::new(),
        100,
        |path| std::fs::remove_dir_all(path),
    );

    assert!(sessions_dir.join("bad-lock/meta.json").exists());
}

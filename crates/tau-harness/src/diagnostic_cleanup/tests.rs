use std::path::Path;
use std::time::{Duration, SystemTime};

use fs2::FileExt as _;
use tempfile::TempDir;

use super::{cleanup_diagnostic_jsonl_with, spawn_diagnostic_jsonl_cleanup_for_test};

/// Ensures the startup entry point launches only for durable configured
/// retention and protects the session being opened.
#[test]
fn startup_cleanup_honors_persistence_retention_and_current_session() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    for name in ["current", "old"] {
        let session = sessions.join(name);
        std::fs::create_dir_all(&session).expect("session dir");
        std::fs::write(session.join("events.jsonl"), b"debug").expect("debug JSONL");
    }
    let current = tau_proto::SessionId::parse("current").expect("session id");

    spawn_diagnostic_jsonl_cleanup_for_test(
        sessions.clone(),
        Some(Duration::ZERO),
        tau_core::SessionPersistenceMode::Durable,
        vec![current],
    )
    .expect("cleanup spawn")
    .expect("durable configured cleanup launches")
    .join()
    .expect("cleanup thread");

    assert!(sessions.join("current/events.jsonl").exists());
    assert!(!sessions.join("old/events.jsonl").exists());
    assert!(
        spawn_diagnostic_jsonl_cleanup_for_test(
            sessions.clone(),
            None,
            tau_core::SessionPersistenceMode::Durable,
            Vec::new(),
        )
        .expect("disabled cleanup does not spawn")
        .is_none()
    );
    assert!(
        spawn_diagnostic_jsonl_cleanup_for_test(
            sessions,
            Some(Duration::ZERO),
            tau_core::SessionPersistenceMode::Ephemeral,
            Vec::new(),
        )
        .expect("ephemeral cleanup does not spawn")
        .is_none()
    );
}

/// Ensures cleanup removes only the known non-authoritative JSONL filename
/// and leaves canonical journals and unrelated diagnostic files untouched.
#[test]
fn cleanup_scope_is_limited_to_session_events_jsonl() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let session = sessions.join("one");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(session.join("events.jsonl"), b"debug").expect("debug JSONL");
    std::fs::write(session.join("events.cbor"), b"canonical").expect("canonical journal");
    std::fs::write(session.join("other.jsonl"), b"private").expect("unrelated JSONL");

    cleanup_diagnostic_jsonl_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| std::fs::remove_file(path),
    );

    assert!(!session.join("events.jsonl").exists());
    assert!(session.join("events.cbor").exists());
    assert!(session.join("other.jsonl").exists());
}

/// Ensures a diagnostic newer than the configured window remains available.
#[test]
fn cleanup_keeps_recent_diagnostic_jsonl() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let session = sessions.join("recent");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(session.join("events.jsonl"), b"debug").expect("debug JSONL");

    cleanup_diagnostic_jsonl_with(
        &sessions,
        Duration::from_secs(14 * 24 * 60 * 60),
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| std::fs::remove_file(path),
    );

    assert!(session.join("events.jsonl").exists());
}

/// Ensures cleanup does not follow a symlink that appears in the session
/// directory namespace.
#[cfg(unix)]
#[test]
fn cleanup_preserves_symlinked_session_directory_target() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let external = temp.path().join("external");
    std::fs::create_dir_all(&sessions).expect("sessions dir");
    std::fs::create_dir_all(&external).expect("external dir");
    std::fs::write(external.join("events.jsonl"), b"external").expect("external JSONL");
    symlink(&external, sessions.join("linked")).expect("session symlink");

    cleanup_diagnostic_jsonl_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| std::fs::remove_file(path),
    );

    assert!(external.join("events.jsonl").exists());
}

/// Ensures cleanup does not follow an `events.jsonl` symlink to an unrelated
/// regular file.
#[cfg(unix)]
#[test]
fn cleanup_preserves_symlinked_diagnostic_target() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let session = sessions.join("one");
    let external = temp.path().join("external.jsonl");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(&external, b"external").expect("external JSONL");
    symlink(&external, session.join("events.jsonl")).expect("diagnostic symlink");

    cleanup_diagnostic_jsonl_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| std::fs::remove_file(path),
    );

    assert!(external.exists());
    assert!(session.join("events.jsonl").is_symlink());
}

/// Ensures a removal failure for one session does not prevent cleanup from
/// attempting later independent diagnostic files.
#[test]
fn cleanup_isolates_per_file_removal_errors() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    for name in ["one", "two"] {
        let session = sessions.join(name);
        std::fs::create_dir_all(&session).expect("session dir");
        std::fs::write(session.join("events.jsonl"), b"debug").expect("debug JSONL");
    }
    let mut attempts = Vec::new();

    cleanup_diagnostic_jsonl_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| {
            attempts.push(path.to_path_buf());
            if path.parent().and_then(Path::file_name) == Some("one".as_ref()) {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))
            } else {
                std::fs::remove_file(path)
            }
        },
    );

    assert_eq!(attempts.len(), 2);
    assert!(sessions.join("one/events.jsonl").exists());
    assert!(!sessions.join("two/events.jsonl").exists());
}

/// Ensures cleanup skips the current session and sessions locked by another
/// harness while still removing an independent expired diagnostic.
#[test]
fn cleanup_skips_protected_and_locked_sessions() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    for name in ["current", "locked", "old"] {
        let session = sessions.join(name);
        std::fs::create_dir_all(&session).expect("session dir");
        std::fs::write(session.join("events.jsonl"), b"debug").expect("debug JSONL");
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(sessions.join("locked/lock"))
        .expect("lock file");
    lock.try_lock_exclusive().expect("hold session lock");
    let current = tau_proto::SessionId::parse("current").expect("session id");

    cleanup_diagnostic_jsonl_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[current],
        |path| std::fs::remove_file(path),
    );

    assert!(sessions.join("current/events.jsonl").exists());
    assert!(sessions.join("locked/events.jsonl").exists());
    assert!(!sessions.join("old/events.jsonl").exists());
    lock.unlock().expect("unlock");
}

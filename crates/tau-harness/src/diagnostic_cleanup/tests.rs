use std::path::Path;
use std::time::{Duration, SystemTime};
use std::{fs as path_std_fs, io as path_std_io};

use fs2::FileExt as _;
use tempfile::TempDir;

use super::{
    cleanup_diagnostics_with, cleanup_diagnostics_with_lock, spawn_diagnostic_cleanup_for_test,
    try_acquire_diagnostic_cleanup_lock,
};

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

    spawn_diagnostic_cleanup_for_test(
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
        spawn_diagnostic_cleanup_for_test(
            sessions.clone(),
            None,
            tau_core::SessionPersistenceMode::Durable,
            Vec::new(),
        )
        .expect("disabled cleanup does not spawn")
        .is_none()
    );
    assert!(
        spawn_diagnostic_cleanup_for_test(
            sessions,
            Some(Duration::ZERO),
            tau_core::SessionPersistenceMode::Ephemeral,
            Vec::new(),
        )
        .expect("ephemeral cleanup does not spawn")
        .is_none()
    );
}

/// Ensures cleanup removes compressed provider captures alongside the JSONL
/// mirror while preserving canonical, legacy, and unrelated files.
#[test]
fn cleanup_scope_is_limited_to_known_diagnostic_paths_and_names() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let session = sessions.join("one");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(session.join("events.jsonl"), b"debug").expect("debug JSONL");
    std::fs::write(session.join("events.cbor"), b"canonical").expect("canonical journal");
    std::fs::write(session.join("other.jsonl"), b"private").expect("unrelated JSONL");
    let captures = session.join("debug/provider-requests");
    std::fs::create_dir_all(&captures).expect("capture dir");
    for name in [
        "3-prompt-websocket-request.json.zst",
        "4-prompt-websocket-response.json.zst",
    ] {
        std::fs::write(captures.join(name), b"capture").expect("provider capture");
    }
    for name in [
        "unrelated.json",
        "1-prompt-http-sse-request.json",
        "2-prompt-http-sse-response.json",
        "request.json",
        "x-response.json",
        "x-request.json.gz",
        "1-prompt-websocket-response.json.zstd",
    ] {
        std::fs::write(captures.join(name), b"unrelated").expect("unrelated capture");
    }
    std::fs::write(session.join("debug/canonical.cbor"), b"canonical").expect("nested canonical");

    cleanup_diagnostics_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| std::fs::remove_file(path),
    );

    assert!(!session.join("events.jsonl").exists());
    assert!(session.join("events.cbor").exists());
    assert!(session.join("other.jsonl").exists());
    assert!(session.join("debug/canonical.cbor").exists());
    for name in [
        "3-prompt-websocket-request.json.zst",
        "4-prompt-websocket-response.json.zst",
    ] {
        assert!(!captures.join(name).exists(), "{name} must be removed");
    }
    for name in [
        "unrelated.json",
        "1-prompt-http-sse-request.json",
        "2-prompt-http-sse-response.json",
        "request.json",
        "x-response.json",
        "x-request.json.gz",
        "1-prompt-websocket-response.json.zstd",
    ] {
        assert!(captures.join(name).exists(), "{name} must remain");
    }
}

/// Ensures retention cleanup reaches provider-instance capture sinks without
/// treating an instance directory's unrelated files as diagnostics.
#[test]
fn cleanup_removes_expired_nested_provider_instance_captures() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let captures = sessions.join("one/debug/provider-requests/provider-work");
    std::fs::create_dir_all(&captures).expect("instance capture dir");
    let capture = captures.join("1-prompt-http-sse-request.json.zst");
    let unrelated = captures.join("notes.json");
    std::fs::write(&capture, b"capture").expect("provider capture");
    std::fs::write(&unrelated, b"unrelated").expect("unrelated file");

    cleanup_diagnostics_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| std::fs::remove_file(path),
    );

    assert!(!capture.exists());
    assert!(unrelated.exists());
}

/// Ensures a diagnostic newer than the configured window remains available.
#[test]
fn cleanup_keeps_recent_diagnostic_jsonl() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let session = sessions.join("recent");
    std::fs::create_dir_all(&session).expect("session dir");
    std::fs::write(session.join("events.jsonl"), b"debug").expect("debug JSONL");

    cleanup_diagnostics_with(
        &sessions,
        Duration::from_secs(14 * 24 * 60 * 60),
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| std::fs::remove_file(path),
    );

    assert!(session.join("events.jsonl").exists());
}

/// Ensures a session removed after enumeration silently skips lock acquisition
/// without recreating it or preventing cleanup of an independent session.
#[test]
fn cleanup_skips_session_that_vanishes_after_enumeration() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let vanished = sessions.join("vanished");
    let retained = sessions.join("retained");
    for session in [&vanished, &retained] {
        std::fs::create_dir_all(session).expect("session dir");
        std::fs::write(session.join("events.jsonl"), b"debug").expect("debug JSONL");
    }
    let mut removed_vanished = false;

    cleanup_diagnostics_with_lock(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        &mut |path| std::fs::remove_file(path),
        |session_dir| {
            let vanishes_before_lock = session_dir == vanished && !removed_vanished;
            if vanishes_before_lock {
                std::fs::remove_dir_all(&vanished).expect("remove enumerated session");
                removed_vanished = true;
            }
            match try_acquire_diagnostic_cleanup_lock(session_dir) {
                Ok(lock) => {
                    if vanishes_before_lock {
                        assert!(lock.is_none(), "vanished session must silently skip");
                    }
                    Ok(lock)
                }
                Err(error) if vanishes_before_lock => {
                    panic!("vanished session lock acquisition: {error}");
                }
                Err(error) => Err(error),
            }
        },
    );

    assert!(removed_vanished);
    assert!(!vanished.exists());
    assert!(!retained.join("events.jsonl").exists());
}

/// Ensures an existing session with a dangling lock path remains an acquisition
/// error instead of being mistaken for a session that vanished after
/// enumeration.
#[cfg(unix)]
#[test]
fn dangling_session_lock_remains_an_acquisition_error() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp state");
    let session = temp.path().join("session");
    std::fs::create_dir_all(&session).expect("session dir");
    symlink("missing-parent/lock", session.join("lock")).expect("dangling lock symlink");

    let error = try_acquire_diagnostic_cleanup_lock(&session).expect_err("dangling lock fails");

    assert_eq!(error.kind(), path_std_io::ErrorKind::NotFound);
}

/// Ensures the shared cutoff removes JSONL and compressed captures at the exact
/// boundary while keeping each one just below it.
#[test]
fn cleanup_applies_exact_shared_cutoff_to_every_diagnostic_class() {
    let retention = Duration::from_secs(60);
    for relative in [
        "events.jsonl",
        "debug/provider-requests/1-prompt-websocket-response.json.zst",
        "debug/provider-requests/1-prompt-compact-http-failure.json.zst",
    ] {
        for (label, age, removed) in [
            ("exact", retention, true),
            ("younger", retention - Duration::from_secs(1), false),
        ] {
            let temp = TempDir::new().expect("temp state");
            let sessions = temp.path().join("sessions");
            let path = sessions.join(label).join(relative);
            std::fs::create_dir_all(path.parent().expect("diagnostic parent"))
                .expect("diagnostic parent");
            std::fs::write(&path, b"diagnostic").expect("diagnostic");
            let modified = std::fs::symlink_metadata(&path)
                .expect("diagnostic metadata")
                .modified()
                .expect("modified time");

            cleanup_diagnostics_with(&sessions, retention, modified + age, &[], |path| {
                std::fs::remove_file(path)
            });

            assert_eq!(!path.exists(), removed, "{relative} at {label} cutoff");
        }
    }
}

/// Ensures cleanup delegates capture recognition to the shared filename
/// contract rather than maintaining a second grammar.
#[test]
fn cleanup_uses_shared_provider_capture_filename_contract() {
    assert!(super::is_provider_capture_filename(
        "123-sp-6-http-sse-request.json.zst"
    ));
    assert!(super::is_provider_capture_filename(
        "123-sp-6-compact-http-failure.json.zst"
    ));
    assert!(!super::is_provider_capture_filename(
        "notes-http-sse-request.json"
    ));
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

    cleanup_diagnostics_with(
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

    cleanup_diagnostics_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| std::fs::remove_file(path),
    );

    assert!(external.exists());
    assert!(session.join("events.jsonl").is_symlink());
}

/// Ensures cleanup does not traverse symlinked debug or provider-capture
/// directories and does not remove a symlink with a capture-like filename.
#[cfg(unix)]
#[test]
fn cleanup_preserves_symlinked_capture_paths_and_targets() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let external = temp.path().join("external");
    std::fs::create_dir_all(&external).expect("external dir");
    let external_capture = external.join("1-prompt-http-sse-request.json.zst");
    std::fs::write(&external_capture, b"external").expect("external capture");

    let linked_debug = sessions.join("linked-debug");
    std::fs::create_dir_all(&linked_debug).expect("linked-debug session");
    symlink(&external, linked_debug.join("debug")).expect("debug symlink");

    let linked_capture_dir = sessions.join("linked-capture/debug");
    std::fs::create_dir_all(&linked_capture_dir).expect("linked-capture debug");
    symlink(&external, linked_capture_dir.join("provider-requests"))
        .expect("provider capture symlink");

    let linked_file_dir = sessions.join("linked-file/debug/provider-requests");
    std::fs::create_dir_all(&linked_file_dir).expect("linked-file capture dir");
    symlink(
        &external_capture,
        linked_file_dir.join("1-prompt-websocket-response.json.zst"),
    )
    .expect("capture file symlink");

    cleanup_diagnostics_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| std::fs::remove_file(path),
    );

    assert!(external_capture.exists());
    assert!(linked_debug.join("debug").is_symlink());
    assert!(linked_capture_dir.join("provider-requests").is_symlink());
    assert!(
        linked_file_dir
            .join("1-prompt-websocket-response.json.zst")
            .is_symlink()
    );
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

    cleanup_diagnostics_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| {
            attempts.push(path.to_path_buf());
            if path.parent().and_then(Path::file_name) == Some("one".as_ref()) {
                Err(path_std_io::Error::new(
                    path_std_io::ErrorKind::PermissionDenied,
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

/// Ensures one failed removal does not suppress a later capture in the same
/// provider diagnostic directory.
#[test]
fn cleanup_isolates_removal_errors_within_one_session() {
    let temp = TempDir::new().expect("temp state");
    let sessions = temp.path().join("sessions");
    let captures = sessions.join("one/debug/provider-requests");
    std::fs::create_dir_all(&captures).expect("capture dir");
    let first = captures.join("1-prompt-http-sse-request.json.zst");
    let second = captures.join("2-prompt-unknown-response.json.zst");
    std::fs::write(&first, b"first").expect("first capture");
    std::fs::write(&second, b"second").expect("second capture");
    let mut attempts = Vec::new();

    cleanup_diagnostics_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[],
        |path| {
            attempts.push(path.to_path_buf());
            if path == first {
                Err(path_std_io::Error::new(
                    path_std_io::ErrorKind::PermissionDenied,
                    "denied",
                ))
            } else {
                std::fs::remove_file(path)
            }
        },
    );

    attempts.sort();
    let mut expected = vec![first.clone(), second.clone()];
    expected.sort();
    assert_eq!(attempts, expected);
    assert!(first.exists());
    assert!(!second.exists());
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
        let captures = session.join("debug/provider-requests");
        std::fs::create_dir_all(&captures).expect("capture dir");
        std::fs::write(
            captures.join("1-prompt-http-sse-request.json.zst"),
            b"capture",
        )
        .expect("provider capture");
    }
    let lock = path_std_fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(sessions.join("locked/lock"))
        .expect("lock file");
    lock.try_lock_exclusive().expect("hold session lock");
    let current = tau_proto::SessionId::parse("current").expect("session id");

    cleanup_diagnostics_with(
        &sessions,
        Duration::ZERO,
        SystemTime::now() + Duration::from_secs(1),
        &[current],
        |path| std::fs::remove_file(path),
    );

    assert!(sessions.join("current/events.jsonl").exists());
    assert!(sessions.join("locked/events.jsonl").exists());
    assert!(!sessions.join("old/events.jsonl").exists());
    assert!(
        sessions
            .join("current/debug/provider-requests/1-prompt-http-sse-request.json.zst")
            .exists()
    );
    assert!(
        sessions
            .join("locked/debug/provider-requests/1-prompt-http-sse-request.json.zst")
            .exists()
    );
    assert!(
        !sessions
            .join("old/debug/provider-requests/1-prompt-http-sse-request.json.zst")
            .exists()
    );
    lock.unlock().expect("unlock");
}

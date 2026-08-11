use super::*;

/// Ensures cleanup removes an old artifact once 32 later relevant ext-shell
/// executions have occurred.
#[test]
fn saved_output_expires_after_later_call_threshold() {
    let (directory, owner_lock) = create_private_directory().expect("private directory");
    let path = directory.join(FILE_NAME);
    write_private_file(&path, b"ordered output").expect("write output");
    let mut tracker = Tracker::default();
    tracker.track(path.clone(), owner_lock);
    tracker.files[0].created = SystemTime::now() - MAX_AGE;
    for _ in 1..MAX_LATER_CALLS {
        tracker.note_call();
    }
    assert!(path.exists(), "artifact must survive 31 later calls");
    tracker.note_call();
    assert!(!path.exists(), "artifact must expire on call 32");
}

/// Ensures graceful shutdown removes every artifact in an isolated tracker.
#[test]
fn graceful_shutdown_removes_tracked_output() {
    let (directory, owner_lock) = create_private_directory().expect("private directory");
    let path = directory.join(FILE_NAME);
    write_private_file(&path, b"ordered output").expect("write output");
    let mut tracker = Tracker::default();
    tracker.track(path.clone(), owner_lock);
    tracker.remove_all();
    assert!(!path.exists());
}

/// Ensures call volume alone cannot expire a young artifact before the age
/// threshold is also satisfied.
#[test]
fn saved_output_requires_both_age_and_call_thresholds() {
    let (directory, owner_lock) = create_private_directory().expect("private directory");
    let path = directory.join(FILE_NAME);
    write_private_file(&path, b"ordered output").expect("write output");
    let mut tracker = Tracker::default();
    tracker.track(path.clone(), owner_lock);
    for _ in 0..MAX_LATER_CALLS {
        tracker.note_call();
    }
    assert!(path.exists());
    tracker.remove_all();
}

/// Ensures first-call crash cleanup removes only an old Tau-owned artifact
/// whose owner lock is absent, not a live or unrelated temporary directory.
#[test]
fn crash_leftover_cleanup_removes_only_old_owned_dead_artifact() {
    let temporary_directory = tempfile::tempdir().expect("temporary directory");
    let dead_directory = temporary_directory
        .path()
        .join(format!("{DIRECTORY_PREFIX}dead"));
    let live_directory = temporary_directory
        .path()
        .join(format!("{DIRECTORY_PREFIX}live"));
    let unrelated_directory = temporary_directory.path().join("unrelated");
    for directory in [&dead_directory, &live_directory, &unrelated_directory] {
        fs::create_dir(directory).expect("artifact directory");
        write_private_file(&directory.join(FILE_NAME), b"ordered output").expect("write output");
    }

    let live_lock_path = live_directory.join(LOCK_FILE_NAME);
    write_private_file(&live_lock_path, b"").expect("write live owner lock");
    let live_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(live_lock_path)
        .expect("open live owner lock");
    fs2::FileExt::lock_exclusive(&live_lock).expect("lock live owner");

    cleanup_crash_leftovers_in(
        temporary_directory.path(),
        SystemTime::now()
            .checked_add(MAX_AGE)
            .expect("future cleanup time"),
    );

    assert!(
        !dead_directory.exists(),
        "dead owned artifact must be removed"
    );
    assert!(live_directory.exists(), "live owned artifact must remain");
    assert!(
        unrelated_directory.exists(),
        "unrelated temporary directory must remain"
    );
}

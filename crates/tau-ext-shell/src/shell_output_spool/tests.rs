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

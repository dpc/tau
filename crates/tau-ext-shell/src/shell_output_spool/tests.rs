use super::*;

/// The prefix adapter preserves complete/incomplete fields and shares the
/// existing unavailable-storage warning path rather than claiming completeness.
#[test]
fn bounded_prefix_metadata_reports_true_completeness_and_save_failure() {
    use tau_proto::CborValue;
    for incomplete in [false, true] {
        let mut entries = Vec::new();
        append_prefix_metadata(&mut entries, "1 content", incomplete);
        let field = |name: &str| {
            entries.iter().find_map(|(key, value)| {
                (key == &CborValue::Text(name.to_owned())).then_some(value)
            })
        };
        let path_field = if incomplete {
            "saved_output_path"
        } else {
            "full_output_path"
        };
        let Some(CborValue::Text(path)) = field(path_field) else {
            panic!("missing saved artifact");
        };
        assert_eq!(
            fs::read_to_string(path).expect("read saved output"),
            "1 content"
        );
        assert_eq!(
            field("saved_output_truncated"),
            incomplete.then_some(&CborValue::Bool(true))
        );
        assert_eq!(
            field("saved_output_bytes"),
            incomplete.then_some(&CborValue::Integer(9.into()))
        );
        assert!(field("truncation_warning").is_some());
        assert!(field("saved_output_unavailable").is_none());
    }
    // Force the existing save boundary to fail without modifying global temp
    // environment or depending on host permissions.
    let mut entries = Vec::new();
    append_prefix_metadata(&mut entries, &"x".repeat(MAX_SAVED_OUTPUT_BYTES + 1), true);
    assert_eq!(
        entries,
        vec![
            (
                CborValue::Text("truncation_warning".to_owned()),
                CborValue::Text(
                    "Fetching excessive output is inefficient; prefer narrower ranges or filters."
                        .to_owned()
                )
            ),
            (
                CborValue::Text("saved_output_unavailable".to_owned()),
                CborValue::Bool(true)
            ),
        ]
    );
}

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

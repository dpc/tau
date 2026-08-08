use std::fs as path_std_fs;

use super::*;

/// Ensures extension data reads reject oversized files before allocating the
/// whole contents on the harness request path.
#[test]
fn read_file_rejects_files_larger_than_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("too-large.bin");
    path_std_fs::File::create(&file_path)
        .expect("create file")
        .set_len(EXTENSION_DATA_MAX_FILE_BYTES + 1)
        .expect("make sparse oversized file");

    let err = run_extension_data_read_file(tempdir.path(), "too-large.bin".to_owned())
        .expect_err("oversized read must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
}

/// Ensures extension data writes refuse payloads that would exceed the
/// harness-enforced disk quota for a single extension-owned file.
#[test]
fn write_file_rejects_payloads_larger_than_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let contents = vec![0; EXTENSION_DATA_MAX_FILE_BYTES as usize + 1];

    let err = run_extension_data_write_file(tempdir.path(), "too-large.bin".to_owned(), contents)
        .expect_err("oversized write must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
    assert!(!tempdir.path().join("too-large.bin").exists());
}

/// Ensures exclusive create enforces the same single-file quota as replace
/// writes and leaves no destination file after refusing the payload.
#[test]
fn create_file_rejects_payloads_larger_than_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let contents = vec![0; EXTENSION_DATA_MAX_FILE_BYTES as usize + 1];

    let err = run_extension_data_create_file(tempdir.path(), "too-large.bin".to_owned(), contents)
        .expect_err("oversized create must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
    assert!(!tempdir.path().join("too-large.bin").exists());
}

/// Ensures appending to an existing file cannot grow extension data beyond the
/// single-file quota even when each individual append request is small.
#[test]
fn append_file_rejects_growth_beyond_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let file_path = tempdir.path().join("nearly-full.bin");
    path_std_fs::File::create(&file_path)
        .expect("create file")
        .set_len(EXTENSION_DATA_MAX_FILE_BYTES)
        .expect("make sparse quota-sized file");

    let err = run_extension_data_append_file(tempdir.path(), "nearly-full.bin".to_owned(), vec![0])
        .expect_err("append beyond quota must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
}

/// Ensures a User-scope append waits for the per-instance lock, so its quota
/// check and synchronous append cannot race another harness process sharing the
/// same root.
#[test]
fn user_scope_append_serializes_on_the_extension_root_lock() {
    use std::sync::mpsc;
    use std::time::Duration;

    use fs2::FileExt as _;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let root = tempdir.path().join("user");
    run_user_extension_data_append_file(&root, "papercuts.jsonl".to_owned(), b"first\n".to_vec())
        .expect("first append");
    let lock = path_std_fs::File::open(&root).expect("open extension root");
    lock.lock_exclusive().expect("hold extension root lock");

    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let append_root = root.clone();
    let append = std::thread::spawn(move || {
        started_tx.send(()).expect("report append start");
        let result = run_user_extension_data_append_file(
            &append_root,
            "papercuts.jsonl".to_owned(),
            b"second\n".to_vec(),
        );
        finished_tx.send(result).expect("report append result");
    });

    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("append worker start");
    assert!(
        finished_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "User append must wait for the held extension root lock"
    );
    fs2::FileExt::unlock(&lock).expect("release extension root lock");
    assert!(
        finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("append completion")
            .is_ok()
    );
    append.join().expect("append thread");
    assert_eq!(
        std::fs::read(root.join("papercuts.jsonl")).expect("read appends"),
        b"first\nsecond\n"
    );
}

/// Ensures directory listing has a hard collection cap before sorting entries
/// for extension-controlled data.
#[test]
fn list_files_rejects_directories_larger_than_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    for index in 0..=MAX_EXTENSION_DATA_LIST_ENTRIES {
        std::fs::write(tempdir.path().join(format!("entry-{index}")), b"")
            .expect("create list entry");
    }

    let err = run_extension_data_list_files(tempdir.path(), String::new())
        .expect_err("oversized list must fail");

    assert_eq!(err.kind, tau_proto::ExtensionDataErrorKind::QuotaExceeded);
}

/// Proves CAS replaces only an exact generation and rejects a stale writer.
#[test]
fn compare_and_swap_replaces_only_the_expected_generation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("secret");
    run_extension_data_write_file_with_limit(
        &root,
        "providers/chatgpt/oauth.json".to_owned(),
        b"first".to_vec(),
        MAX_SECRET_DATA_FILE_BYTES,
    )
    .expect("initial write");
    let first_generation = blake3::hash(b"first").to_hex().to_string();

    assert!(matches!(
        run_extension_data_compare_and_swap_file(
            &root,
            "providers/chatgpt/oauth.json".to_owned(),
            first_generation.clone(),
            b"second".to_vec(),
            MAX_SECRET_DATA_FILE_BYTES,
        ),
        Ok(tau_proto::ExtensionDataValue::CompareAndSwapFile)
    ));
    let error = run_extension_data_compare_and_swap_file(
        &root,
        "providers/chatgpt/oauth.json".to_owned(),
        first_generation,
        b"stale".to_vec(),
        MAX_SECRET_DATA_FILE_BYTES,
    )
    .expect_err("stale generation rejected");
    assert_eq!(
        error.kind,
        tau_proto::ExtensionDataErrorKind::GenerationMismatch
    );
    assert_eq!(
        std::fs::read(root.join("providers/chatgpt/oauth.json")).expect("read"),
        b"second"
    );
}

/// Proves CAS never follows a credential leaf symlink outside its scope.
#[cfg(unix)]
#[test]
fn compare_and_swap_rejects_a_symlink_leaf() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("secret");
    std::fs::create_dir_all(&root).expect("root");
    let outside = temp.path().join("outside");
    std::fs::write(&outside, "outside").expect("outside");
    symlink(&outside, root.join("credential.json")).expect("symlink");

    let error = run_extension_data_compare_and_swap_file(
        &root,
        "credential.json".to_owned(),
        blake3::hash(b"outside").to_hex().to_string(),
        b"replacement".to_vec(),
        MAX_SECRET_DATA_FILE_BYTES,
    )
    .expect_err("symlink rejected");
    assert_eq!(error.kind, tau_proto::ExtensionDataErrorKind::InvalidPath);
    assert_eq!(std::fs::read(outside).expect("outside"), b"outside");
}

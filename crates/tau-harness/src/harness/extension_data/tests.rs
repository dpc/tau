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

/// Ensures Session-scope append dispatch waits for its exact scope-root lock,
/// so requested-path validation, quota checking, and writing stay inside the
/// cooperative critical section.
#[test]
fn session_scope_append_dispatch_serializes_on_the_scope_root_lock() {
    use std::sync::mpsc;
    use std::time::Duration;

    use fs2::FileExt as _;

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let root = tempdir.path().join("session");
    path_std_fs::create_dir_all(&root).expect("create scope root");
    let path = root.join("quota");
    path_std_fs::File::create(&path)
        .expect("create quota file")
        .set_len(EXTENSION_DATA_MAX_FILE_BYTES - 1)
        .expect("reserve all but one quota byte");
    let lock = path_std_fs::File::open(&root).expect("open extension root");
    lock.lock_exclusive().expect("hold extension root lock");

    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let append_root = root.clone();
    let append = std::thread::spawn(move || {
        started_tx.send(()).expect("report append start");
        let append = || {
            run_scoped_extension_data_append_file(
                tau_proto::ExtensionDataScope::Session,
                &append_root,
                "quota".to_owned(),
                vec![1],
            )
        };
        finished_tx
            .send([append(), append()])
            .expect("report append results");
    });

    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("append worker start");
    assert!(
        finished_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "Session append dispatch must wait for the held scope root lock"
    );
    fs2::FileExt::unlock(&lock).expect("release extension root lock");
    let results = finished_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("append completion");
    append.join().expect("append thread");
    assert!(results[0].is_ok());
    assert_eq!(
        results[1].as_ref().expect_err("quota rejection").kind,
        tau_proto::ExtensionDataErrorKind::QuotaExceeded
    );
    assert_eq!(
        path_std_fs::metadata(path).expect("quota file").len(),
        EXTENSION_DATA_MAX_FILE_BYTES
    );
}

/// Preserves the append RPC's deliberately non-idempotent retry boundary: the
/// harness does not recognize repeated bytes as a duplicate request.
#[test]
fn locked_append_retry_can_duplicate_bytes() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let root = tempdir.path().join("session");

    for _ in 0..2 {
        run_locked_extension_data_append_file(&root, "retry.log".to_owned(), b"record\n".to_vec())
            .expect("append attempt");
    }

    assert_eq!(
        path_std_fs::read(root.join("retry.log")).expect("retry log"),
        b"record\nrecord\n"
    );
}

/// Preserves the explicitly ambiguous failure boundary: append does not roll
/// back bytes after a write, file-sync, or new-file parent-sync failure.
#[test]
fn append_failure_can_leave_partial_or_complete_bytes() {
    use std::io::{Error, Write as _};

    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let partial = tempdir.path().join("partial");
    let error = append_extension_data_file_with(
        &partial,
        b"complete",
        |mut file, _| {
            file.write_all(b"part")?;
            Err(Error::other("injected write failure"))
        },
        |_| unreachable!("write failure skips parent sync"),
    )
    .expect_err("injected write failure");
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(path_std_fs::read(partial).expect("partial append"), b"part");

    let file_sync = tempdir.path().join("file-sync");
    append_extension_data_file_with(
        &file_sync,
        b"complete",
        |mut file, contents| {
            file.write_all(contents)?;
            Err(Error::other("injected file sync failure"))
        },
        |_| unreachable!("file sync failure skips parent sync"),
    )
    .expect_err("injected file sync failure");
    assert_eq!(
        path_std_fs::read(file_sync).expect("complete append before file sync failure"),
        b"complete"
    );

    let parent_sync = tempdir.path().join("parent-sync");
    append_extension_data_file_with(&parent_sync, b"complete", write_file_sync, |_| {
        Err(Error::other("injected parent sync failure"))
    })
    .expect_err("injected parent sync failure");
    assert_eq!(
        path_std_fs::read(parent_sync).expect("complete append before parent sync failure"),
        b"complete"
    );
}

/// Ensures directory listing has a hard collection cap before sorting entries
/// for extension-controlled data.
#[test]
fn list_files_rejects_directories_larger_than_extension_data_limit() {
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let test_limit = 3;
    for index in 0..=test_limit {
        std::fs::write(tempdir.path().join(format!("entry-{index}")), b"")
            .expect("create list entry");
    }

    let err = list_extension_data_entries_with_limit(tempdir.path(), tempdir.path(), test_limit)
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

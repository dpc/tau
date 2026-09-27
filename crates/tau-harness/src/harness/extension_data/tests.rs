use std::fs as path_std_fs;

use super::*;

/// An absent reporter never creates storage, while a locked read returns the
/// exact active bytes (including an intentionally empty existing file).
#[test]
fn papercut_read_preserves_absence_and_complete_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("ext/std-utils");
    assert_eq!(
        run_papercut_read(&root).expect("absent history"),
        tau_proto::ExtensionDataValue::ReadPapercuts { contents: None }
    );
    assert!(!root.exists());
    path_std_fs::create_dir_all(&root).expect("root");
    path_std_fs::write(root.join("papercuts.jsonl"), b"").expect("empty active file");
    assert_eq!(
        run_papercut_read(&root).expect("existing empty history"),
        tau_proto::ExtensionDataValue::ReadPapercuts {
            contents: Some(Vec::new())
        }
    );
}

/// A stale generation cannot archive a concurrent append; archive collision
/// selection preserves previous private bytes and leaves fresh appends active.
#[test]
fn papercut_archive_rejects_stale_snapshot_and_preserves_collisions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("ext/std-utils");
    path_std_fs::create_dir_all(&root).expect("root");
    let active = root.join("papercuts.jsonl");
    let first = b"{\"schema\":1,\"agent_id\":\"agent-a\",\"session_id\":\"session-a\",\"timestamp_us\":1000000,\"report\":\"first\"}\n";
    path_std_fs::write(&active, first).expect("active");
    let old_generation = blake3::hash(first).to_hex().to_string();
    run_locked_extension_data_append_file(
        &root,
        "papercuts.jsonl".to_owned(),
        b"{\"schema\":1,\"agent_id\":\"agent-b\",\"session_id\":\"session-b\",\"timestamp_us\":2000000,\"report\":\"second\"}\n".to_vec(),
    )
    .expect("concurrent reporter append");
    let error = run_papercut_archive(&root, &old_generation).expect_err("stale generation");
    assert_eq!(
        error.kind,
        tau_proto::ExtensionDataErrorKind::GenerationMismatch
    );
    let new_bytes = path_std_fs::read(&active).expect("active intact");
    let existing = root.join("papercuts.archive-0000000000000001.jsonl");
    path_std_fs::write(&existing, b"preserved").expect("first archive");
    let result = run_papercut_archive(&root, blake3::hash(&new_bytes).to_hex().as_ref())
        .expect("matching snapshot");
    assert_eq!(
        result,
        tau_proto::ExtensionDataValue::ArchivePapercuts {
            archive: tau_proto::ExtensionDataPath::new("papercuts.archive-0000000000000002.jsonl")
        }
    );
    assert_eq!(
        path_std_fs::read(existing).expect("first archive"),
        b"preserved"
    );
    assert_eq!(
        path_std_fs::read(root.join("papercuts.archive-0000000000000002.jsonl"))
            .expect("second archive"),
        new_bytes
    );
    assert!(!active.exists());
    run_locked_extension_data_append_file(&root, "papercuts.jsonl".to_owned(), b"after\n".to_vec())
        .expect("append after archive");
    assert_eq!(path_std_fs::read(active).expect("fresh active"), b"after\n");
}

/// A symlink occupying the first numbered archive path is not replaced or
/// followed; the same existing collision rule also applies to dangling links.
#[cfg(unix)]
#[test]
fn papercut_archive_skips_occupied_symlink_without_changing_its_target() {
    use std::os::unix::fs as unix_fs;

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("ext/std-utils");
    path_std_fs::create_dir_all(&root).expect("root");
    let active = root.join("papercuts.jsonl");
    path_std_fs::write(&active, b"validated\n").expect("active");
    let link = root.join("papercuts.archive-0000000000000001.jsonl");
    unix_fs::symlink("missing-target", &link).expect("dangling occupied archive");
    let result = run_papercut_archive(&root, blake3::hash(b"validated\n").to_hex().as_ref())
        .expect("next archive");
    assert_eq!(
        result,
        tau_proto::ExtensionDataValue::ArchivePapercuts {
            archive: tau_proto::ExtensionDataPath::new("papercuts.archive-0000000000000002.jsonl")
        }
    );
    assert!(
        path_std_fs::symlink_metadata(link)
            .expect("link intact")
            .file_type()
            .is_symlink()
    );
}

/// A symlinked active reporter file fails before reading or archiving arbitrary
/// data outside the authenticated extension directory.
#[cfg(unix)]
#[test]
fn papercut_read_and_archive_reject_symlinked_active_file() {
    use std::os::unix::fs as unix_fs;

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("ext/std-utils");
    path_std_fs::create_dir_all(&root).expect("root");
    let outside = temp.path().join("outside");
    path_std_fs::write(&outside, b"not reporter data").expect("outside");
    unix_fs::symlink(&outside, root.join("papercuts.jsonl")).expect("symlink");
    assert!(run_papercut_read(&root).is_err());
    assert!(
        run_papercut_archive(&root, blake3::hash(b"not reporter data").to_hex().as_ref()).is_err()
    );
    assert_eq!(
        path_std_fs::read(outside).expect("outside intact"),
        b"not reporter data"
    );
}

/// Racing an append with conditional archival cannot silently lose either
/// report: an append before the archive makes the old generation stale, while
/// an append afterward creates a fresh active file.
#[test]
fn papercut_archive_and_append_race_has_no_lost_report() {
    use std::sync::{Arc, Barrier};

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("ext/std-utils");
    path_std_fs::create_dir_all(&root).expect("root");
    let first = b"first\n";
    let second = b"second\n";
    path_std_fs::write(root.join("papercuts.jsonl"), first).expect("active");
    let gate = Arc::new(Barrier::new(3));
    let archive_root = root.clone();
    let archive_gate = Arc::clone(&gate);
    let archive = std::thread::spawn(move || {
        archive_gate.wait();
        run_papercut_archive(&archive_root, blake3::hash(first).to_hex().as_ref())
    });
    let append_root = root.clone();
    let append_gate = Arc::clone(&gate);
    let append = std::thread::spawn(move || {
        append_gate.wait();
        run_locked_extension_data_append_file(
            &append_root,
            "papercuts.jsonl".to_owned(),
            second.to_vec(),
        )
    });
    gate.wait();
    let result = archive.join().expect("archive thread");
    append.join().expect("append thread").expect("append");
    match result {
        Ok(tau_proto::ExtensionDataValue::ArchivePapercuts { archive }) => {
            assert_eq!(
                path_std_fs::read(root.join(archive.as_str())).expect("archive"),
                first
            );
            assert_eq!(
                path_std_fs::read(root.join("papercuts.jsonl")).expect("active"),
                second
            );
        }
        Err(error) if error.kind == tau_proto::ExtensionDataErrorKind::GenerationMismatch => {
            assert_eq!(
                path_std_fs::read(root.join("papercuts.jsonl")).expect("active"),
                [first.as_slice(), second.as_slice()].concat()
            );
        }
        other => panic!("unexpected archive outcome: {other:?}"),
    }
}

/// Proves a successful Secret mutation synchronizes every containing directory
/// from the leaf parent through Tau's state root, including a freshly prepared
/// scope hierarchy that the current mutation did not create.
#[test]
fn secret_mutation_barriers_cover_nested_and_scope_hierarchies() {
    let state = Path::new("/tau-state");
    let root = state.join("secrets/ext/provider-work");
    let nested = root.join("providers/new-provider/auth.json");
    let mut nested_barriers = Vec::new();

    let result = finish_secret_mutation_with(Ok(7), &nested, state, |directory| {
        nested_barriers.push(directory.to_path_buf());
        Ok(())
    })
    .expect("all nested barriers");

    assert_eq!(result, 7);
    assert_eq!(
        nested_barriers,
        [
            root.join("providers/new-provider"),
            root.join("providers"),
            root.clone(),
            state.join("secrets/ext"),
            state.join("secrets"),
            state.to_path_buf(),
        ]
    );

    let direct = root.join("auth.json");
    let mut direct_barriers = Vec::new();
    finish_secret_mutation_with(Ok(()), &direct, state, |directory| {
        direct_barriers.push(directory.to_path_buf());
        Ok(())
    })
    .expect("fresh scope barriers");
    assert_eq!(
        direct_barriers,
        [
            root,
            state.join("secrets/ext"),
            state.join("secrets"),
            state.to_path_buf(),
        ]
    );
}

/// Proves a containing-directory sync failure prevents Secret mutation success
/// and a retry revisits the complete hierarchy instead of relying on which
/// directories its own `mkdir` calls created.
#[test]
fn secret_mutation_barrier_failure_is_returned_and_retry_revisits_existing_dirs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("tau-state");
    let root = state.join("secrets/ext/provider-work");
    let path = root.join("providers/new-provider/auth.json");
    let mut first_attempt = Vec::new();

    let error = write_extension_data_file_with_limit_locked_with(
        &state,
        &root,
        "providers/new-provider/auth.json".to_owned(),
        b"published".to_vec(),
        MAX_SECRET_DATA_FILE_BYTES,
        |directory| {
            first_attempt.push(directory.to_path_buf());
            if directory == root {
                Err(path_std_io::Error::other("injected directory sync failure"))
            } else {
                Ok(())
            }
        },
    )
    .expect_err("directory sync failure prevents success");
    assert_eq!(error.kind, tau_proto::ExtensionDataErrorKind::Io);
    assert_eq!(
        path_std_fs::read(&path).expect("file publication precedes ancestor barriers"),
        b"published"
    );
    assert_eq!(
        first_attempt,
        [
            root.join("providers/new-provider"),
            root.join("providers"),
            root.clone(),
        ]
    );

    let mut retry = Vec::new();
    let result = write_extension_data_file_with_limit_locked_with(
        &state,
        &root,
        "providers/new-provider/auth.json".to_owned(),
        b"published".to_vec(),
        MAX_SECRET_DATA_FILE_BYTES,
        |directory| {
            retry.push(directory.to_path_buf());
            Ok(())
        },
    )
    .expect("retry barriers");
    assert_eq!(result, tau_proto::ExtensionDataValue::WriteFile);
    assert_eq!(
        retry,
        [
            root.join("providers/new-provider"),
            root.join("providers"),
            root.clone(),
            state.join("secrets/ext"),
            state.join("secrets"),
            state.to_path_buf(),
        ]
    );
}

/// Proves a failed file mutation does not run publication barriers or disguise
/// the original mutation error.
#[test]
fn secret_mutation_failure_skips_directory_barriers() {
    let error = path_std_io::Error::other("injected file mutation failure");
    let result = finish_secret_mutation_with::<()>(
        Err(error),
        Path::new("/tau-state/secrets/ext/provider-work/auth.json"),
        Path::new("/tau-state"),
        |_| panic!("failed mutation must skip directory barriers"),
    );

    assert_eq!(
        result.expect_err("mutation failure").to_string(),
        "injected file mutation failure"
    );
}

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
        temp.path(),
        &root,
        "providers/chatgpt/oauth.json".to_owned(),
        b"first".to_vec(),
        MAX_SECRET_DATA_FILE_BYTES,
    )
    .expect("initial write");
    let first_generation = blake3::hash(b"first").to_hex().to_string();

    assert!(matches!(
        run_extension_data_compare_and_swap_file(
            temp.path(),
            &root,
            "providers/chatgpt/oauth.json".to_owned(),
            first_generation.clone(),
            b"second".to_vec(),
            MAX_SECRET_DATA_FILE_BYTES,
        ),
        Ok(tau_proto::ExtensionDataValue::CompareAndSwapFile)
    ));
    let error = run_extension_data_compare_and_swap_file(
        temp.path(),
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
        temp.path(),
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

use std::fs::OpenOptions;
use std::path::Path;

use super::*;

/// Opens one read/write append file containing a committed baseline.
fn baseline_file(path: &Path) -> File {
    std::fs::write(path, b"baseline").expect("write baseline");
    OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .expect("open append file")
}

/// Every possible torn length prefix is removed and leaves the stream available
/// for a successful retry at the same EOF.
#[test]
fn rolls_back_every_length_prefix_failure() {
    for fail_write_at in 0..8 {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("events.cbor");
        let mut file = baseline_file(&path);
        let mut state = FramedAppendState::default();
        state.inject_fault(
            &path,
            AppendFault {
                fail_write_at: Some(fail_write_at),
                ..AppendFault::default()
            },
        );

        let error = state
            .append(&path, &mut file, b"payload")
            .expect_err("prefix write fails");

        assert_eq!(error.to_string(), "injected frame write failure");
        assert_eq!(
            std::fs::read(&path).expect("read rolled back file"),
            b"baseline"
        );
        assert_eq!(
            state
                .append(&path, &mut file, b"retry")
                .expect("retry appends"),
            FrameAppend {
                start_offset: 8,
                end_offset: 21,
            }
        );
    }
}

/// Every payload byte boundary rolls back to the exact baseline EOF.
#[test]
fn rolls_back_every_payload_failure() {
    let payload = b"payload";
    for payload_offset in 0..payload.len() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("events.cbor");
        let mut file = baseline_file(&path);
        let mut state = FramedAppendState::default();
        state.inject_fault(
            &path,
            AppendFault {
                fail_write_at: Some(8 + payload_offset),
                ..AppendFault::default()
            },
        );

        state
            .append(&path, &mut file, payload)
            .expect_err("payload write fails");

        assert_eq!(
            std::fs::read(&path).expect("read rolled back file"),
            b"baseline"
        );
        state
            .ensure_appendable(&path)
            .expect("successful rollback keeps stream appendable");
    }
}

/// Truncation failure poisons the stream and later attempts do not open or
/// mutate it through the append state.
#[test]
fn truncate_failure_poisons_stream() {
    assert_rollback_failure_poisons(AppendFault {
        fail_write_at: Some(3),
        fail_truncate: true,
        ..AppendFault::default()
    });
}

/// A failure before any byte reaches the file needs no rollback and remains
/// retryable even when truncation itself would fail.
#[test]
fn zero_byte_failure_does_not_poison_stream() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("events.cbor");
    let mut file = baseline_file(&path);
    let mut state = FramedAppendState::default();
    state.inject_fault(
        &path,
        AppendFault {
            fail_write_at: Some(0),
            fail_truncate: true,
            ..AppendFault::default()
        },
    );

    state
        .append(&path, &mut file, b"payload")
        .expect_err("append fails before writing");
    state
        .ensure_appendable(&path)
        .expect("unchanged stream remains appendable");
    state
        .append(&path, &mut file, b"retry")
        .expect("retry succeeds");
}

/// Exercises the common poisoning assertions for either rollback operation.
fn assert_rollback_failure_poisons(fault: AppendFault) {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("events.cbor");
    let mut file = baseline_file(&path);
    let mut state = FramedAppendState::default();
    state.inject_fault(&path, fault);

    let original = state
        .append(&path, &mut file, b"payload")
        .expect_err("append fails");
    let bytes_after_failure = std::fs::read(&path).expect("read failed stream");
    let poisoned = state
        .ensure_appendable(&path)
        .expect_err("rollback uncertainty poisons stream");

    assert_eq!(original.to_string(), "injected frame write failure");
    assert!(poisoned.to_string().contains("append disabled"));
    assert_eq!(
        std::fs::read(&path).expect("read untouched poisoned stream"),
        bytes_after_failure
    );
}

/// Prefix recovery truncates every framing, decoding, and semantic corruption
/// class together with a complete valid-looking suffix.
#[test]
fn recovery_discards_every_invalid_suffix_class() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("partial header", vec![1, 2, 3]),
        (
            "partial payload",
            [5_u64.to_le_bytes().as_slice(), &[1, 2]].concat(),
        ),
        (
            "oversized length",
            (MAX_RECORD_BYTES + 1).to_le_bytes().to_vec(),
        ),
        (
            "invalid cbor",
            [1_u64.to_le_bytes().as_slice(), &[0xff]].concat(),
        ),
        (
            "trailing payload bytes",
            [3_u64.to_le_bytes().as_slice(), &[0x61, b'x', 0]].concat(),
        ),
        ("invalid semantics", encoded_frame(&"bad")),
    ];
    for (name, invalid) in cases {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("events.cbor");
        let valid = encoded_frame(&"good");
        let bytes = [
            valid.as_slice(),
            invalid.as_slice(),
            encoded_frame(&"valid-looking suffix").as_slice(),
        ]
        .concat();
        std::fs::write(&path, bytes).expect("write damaged journal");
        let mut state = FramedAppendState::default();
        state.inject_sync_spawn_failure();

        let recovered = state
            .recover(&path, |record: &String| record != "bad")
            .unwrap_or_else(|error| panic!("{name} recovery failed: {error}"));

        assert!(recovered.repaired, "{name}");
        assert_eq!(recovered.records, vec!["good".to_owned()], "{name}");
        assert_eq!(std::fs::read(&path).expect("read repaired journal"), valid);
        assert_eq!(
            state.dirty_target(&path).map(|target| target.end_offset),
            Some(valid.len() as u64),
            "{name}"
        );
    }
}

/// Encodes one complete journal frame.
fn encoded_frame<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut payload = Vec::new();
    ciborium::into_writer(value, &mut payload).expect("encode frame");
    [payload.len().to_le_bytes().as_slice(), payload.as_slice()].concat()
}

/// Relative store roots normalize their filesystem boundary to `.` instead of
/// producing an unusable empty directory path.
#[test]
fn relative_created_directory_coverage_stops_at_store_boundary() {
    assert_eq!(normalized_directory(Path::new("")), PathBuf::from("."));
}

/// An unavailable post-failure EOF probe falls back to exact truncation and
/// remains retryable when that restoration succeeds.
#[test]
fn failed_eof_probe_with_successful_truncate_is_retryable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("events.cbor");
    let mut file = baseline_file(&path);
    let mut state = FramedAppendState::default();
    state.inject_fault(
        &path,
        AppendFault {
            fail_write_at: Some(3),
            fail_seek_after_write: true,
            ..AppendFault::default()
        },
    );
    state
        .append(&path, &mut file, b"payload")
        .expect_err("append fails");
    state
        .append(&path, &mut file, b"retry")
        .expect("restored journal retries");
}

/// An unavailable EOF probe plus failed exact truncation poisons the journal.
#[test]
fn failed_eof_probe_and_truncate_poisons() {
    assert_rollback_failure_poisons(AppendFault {
        fail_write_at: Some(3),
        fail_truncate: true,
        fail_seek_after_write: true,
    });
}

/// Directory-entry debt is queued per branch and a later store lifetime
/// re-covers an existing boundary independently.
#[test]
fn created_directory_debt_is_branch_scoped_and_recovered_across_lifetimes() {
    let mut first = FramedAppendState::default();
    first.inject_sync_spawn_failure();
    first.note_created_directories([PathBuf::from("root/a")]);
    first.note_created_directories([PathBuf::from("root/b")]);
    assert_eq!(
        first
            .dirty_target(Path::new("root/a"))
            .map(|target| target.directories),
        Some([PathBuf::from("root")].into_iter().collect())
    );
    assert_eq!(
        first
            .dirty_target(Path::new("root/b"))
            .map(|target| target.directories),
        Some([PathBuf::from("root")].into_iter().collect())
    );
    drop(first);

    let mut second = FramedAppendState::default();
    second.inject_sync_spawn_failure();
    second.note_directory_boundary(Path::new("root/a"));
    assert_eq!(
        second
            .dirty_target(Path::new("root/a"))
            .map(|target| target.directories),
        Some([PathBuf::from("root")].into_iter().collect())
    );
}

/// Production chain construction retains the deepest created child and every
/// ancestor required for child-before-parent sync.
#[test]
fn nested_created_directory_chain_builds_one_ordered_target() {
    let mut state = FramedAppendState::default();
    state.inject_sync_spawn_failure();
    state.note_created_directories([
        PathBuf::from("root"),
        PathBuf::from("root/a"),
        PathBuf::from("root/a/b"),
    ]);
    let target = state
        .dirty_target(Path::new("root/a/b"))
        .expect("deepest boundary target");
    assert_eq!(
        target.kind,
        crate::journal_sync::SyncTargetKind::DirectoryBoundary
    );
    assert_eq!(
        target.directories,
        [
            PathBuf::from("."),
            PathBuf::from("root"),
            PathBuf::from("root/a"),
        ]
        .into_iter()
        .collect()
    );
}

/// A clean first-append failure retains the fresh journal's parent-entry debt
/// and a later retry advances the same target.
#[test]
fn fresh_journal_debt_survives_clean_first_append_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("events.cbor");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("create journal");
    let mut state = FramedAppendState::default();
    state.inject_sync_spawn_failure();
    state.note_created_journal(&path, &file);
    state.inject_fault(
        &path,
        AppendFault {
            fail_write_at: Some(0),
            ..AppendFault::default()
        },
    );
    state
        .append(&path, &mut file, b"first")
        .expect_err("first append fails cleanly");
    let parent = path.parent().expect("journal parent").to_path_buf();
    assert!(
        state
            .dirty_target(&path)
            .is_some_and(|target| target.end_offset == 0 && target.directories.contains(&parent))
    );
    let appended = state
        .append(&path, &mut file, b"retry")
        .expect("retry appends");
    let target = state.dirty_target(&path).expect("dirty retry target");
    assert_eq!(target.end_offset, appended.end_offset);
    assert_eq!(target.directories, [parent].into_iter().collect());
}

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
            .expect("durable rollback keeps stream appendable");
    }
}

/// A failed commit sync returns its original error, rolls back, and durably
/// syncs before retry.
#[test]
fn rolls_back_commit_sync_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("events.cbor");
    let mut file = baseline_file(&path);
    let mut state = FramedAppendState::default();
    state.inject_fault(
        &path,
        AppendFault {
            fail_commit_sync: true,
            ..AppendFault::default()
        },
    );

    let error = state
        .append(&path, &mut file, b"payload")
        .expect_err("commit sync fails");

    assert_eq!(error.to_string(), "injected data sync failure");
    assert_eq!(
        std::fs::read(&path).expect("read rolled back file"),
        b"baseline"
    );
    state
        .append(&path, &mut file, b"retry")
        .expect("retry succeeds");
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

/// Rollback-sync failure poisons the stream even when truncation restored its
/// visible length.
#[test]
fn rollback_sync_failure_poisons_stream() {
    assert_rollback_failure_poisons(AppendFault {
        fail_write_at: Some(3),
        fail_rollback_sync: true,
        ..AppendFault::default()
    });
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

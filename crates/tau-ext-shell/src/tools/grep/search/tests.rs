//! Deterministic cancellation regressions for in-process grep.

use super::*;

/// Cancellation checks work in buffered reads even when no sink callback fires.
#[test]
fn reader_checks_cancel_on_nonmatching_input() {
    let (sender, receiver) = mpsc::channel();
    let cancellation = Cancellation {
        receiver: Some(receiver),
        cancelled: Cell::new(false),
    };
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let file = tempdir.path().join("empty");
    fs::write(&file, b"unmatched").expect("write");
    let mut reader = CancellableReader {
        file: File::open(file).expect("open"),
        cancellation: &cancellation,
    };
    sender.send(()).expect("cancel");
    let mut buf = [0; 16];
    assert!(reader.read(&mut buf).is_err());
    assert!(cancellation.check());
}

/// Cancellation during the last traversal step must not become a successful
/// empty search.
#[test]
fn traversal_exhaustion_preserves_cancellation() {
    let (sender, receiver) = mpsc::channel();
    let cancellation = Cancellation {
        receiver: Some(receiver),
        cancelled: Cell::new(false),
    };
    let mut entries = std::iter::from_fn(move || {
        sender.send(()).expect("send cancel while walking");
        None::<std::path::PathBuf>
    });
    assert!(next_entry(&mut entries, &cancellation).is_err());
}

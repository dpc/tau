use std::sync::mpsc;
use std::time::Duration;

use super::{CapturedPipe, MAX_CAPTURE_BYTES, ShellCancel};

/// Capture and finish a sequence of arbitrary reader chunks.
fn capture(chunks: &[&[u8]]) -> CapturedPipe {
    let mut captured = CapturedPipe::new();
    for chunk in chunks {
        captured.push_bytes(chunk);
    }
    captured.finish();
    captured
}

/// Assert a partition produces the same valid capture as one unsplit read.
fn assert_valid_partition(bytes: &[u8], chunks: &[&[u8]]) {
    let unsplit = capture(&[bytes]);
    let partitioned = capture(chunks);
    assert_eq!(partitioned.text, unsplit.text);
    assert_eq!(partitioned.valid_utf8, unsplit.valid_utf8);
    assert_eq!(partitioned.stored_bytes, bytes.len());
    assert_eq!(partitioned.bytes, bytes.len());
    assert!(!partitioned.truncated);
    assert!(partitioned.valid_utf8);
}

/// Valid multibyte characters remain intact across every internal chunk
/// boundary, including one-byte reads and the pipe reader's 8192-byte boundary.
#[test]
fn captured_pipe_decodes_valid_utf8_after_all_chunks_arrive() {
    for character in ["é", "€", "🦀"] {
        let bytes = character.as_bytes();
        for split in 1..bytes.len() {
            assert_valid_partition(bytes, &[&bytes[..split], &bytes[split..]]);
        }
        let one_byte_chunks = bytes.iter().map(std::slice::from_ref).collect::<Vec<_>>();
        assert_valid_partition(bytes, &one_byte_chunks);
    }

    let mut around_reader_boundary = vec![b'x'; 8191];
    around_reader_boundary.extend_from_slice("€".as_bytes());
    assert_valid_partition(
        &around_reader_boundary,
        &[
            &around_reader_boundary[..8192],
            &around_reader_boundary[8192..],
        ],
    );
}

/// Invalid sequences and incomplete final characters use the lossy rendering of
/// the complete retained prefix regardless of how reads partition that prefix.
#[test]
fn captured_pipe_invalid_utf8_is_partition_independent() {
    for bytes in [
        &[b'a', 0xff, b'b'][..],
        &[b'a', 0xe2, 0x82][..],
        &[0xf0, 0x9f, 0xa6][..],
    ] {
        let expected = String::from_utf8_lossy(bytes).into_owned();
        for split in 1..bytes.len() {
            let captured = capture(&[&bytes[..split], &bytes[split..]]);
            assert_eq!(captured.text, expected);
            assert!(!captured.valid_utf8);
            assert_eq!(captured.stored_bytes, bytes.len());
            assert_eq!(captured.bytes, bytes.len());
            assert!(!captured.truncated);
        }
        let one_byte_chunks = bytes.iter().map(std::slice::from_ref).collect::<Vec<_>>();
        let captured = capture(&one_byte_chunks);
        assert_eq!(captured.text, expected);
        assert!(!captured.valid_utf8);
    }
}

/// Capture limits count retained raw bytes, preserve exact-fill semantics, and
/// never use discarded continuation or invalid bytes when validating the
/// prefix.
#[test]
fn captured_pipe_preserves_cap_boundary_semantics() {
    let mut complete_at_cap = vec![b'x'; MAX_CAPTURE_BYTES - 2];
    complete_at_cap.extend_from_slice("é".as_bytes());
    let exact = capture(&[&complete_at_cap]);
    assert_eq!(exact.stored_bytes, MAX_CAPTURE_BYTES);
    assert_eq!(exact.bytes, MAX_CAPTURE_BYTES);
    assert!(!exact.truncated);
    assert!(exact.valid_utf8);
    assert!(exact.text.ends_with('é'));

    let mut cut_character = vec![b'x'; MAX_CAPTURE_BYTES - 1];
    cut_character.extend_from_slice("é".as_bytes());
    let same_read = capture(&[&cut_character]);
    assert_eq!(same_read.stored_bytes, MAX_CAPTURE_BYTES);
    assert_eq!(same_read.bytes, MAX_CAPTURE_BYTES + 1);
    assert!(same_read.truncated);
    assert!(!same_read.valid_utf8);
    assert!(same_read.text.ends_with('\u{fffd}'));

    let prefix = &cut_character[..MAX_CAPTURE_BYTES];
    let later_read = capture(&[prefix, &cut_character[MAX_CAPTURE_BYTES..]]);
    assert_eq!(later_read.text, same_read.text);
    assert_eq!(later_read.stored_bytes, MAX_CAPTURE_BYTES);
    assert_eq!(later_read.bytes, MAX_CAPTURE_BYTES + 1);
    assert!(later_read.truncated);
    assert!(!later_read.valid_utf8);

    let full = vec![b'x'; MAX_CAPTURE_BYTES];
    let overflow_invalid = capture(&[&full, &[0xff, 0xfe]]);
    assert_eq!(overflow_invalid.stored_bytes, MAX_CAPTURE_BYTES);
    assert_eq!(overflow_invalid.bytes, MAX_CAPTURE_BYTES + 2);
    assert!(overflow_invalid.truncated);
    assert!(overflow_invalid.valid_utf8);
    assert_eq!(overflow_invalid.text.as_bytes(), full);
}

/// Ensures cancellation recorded before the watcher starts is still observed so
/// shell shutdown cannot lose an early cancellation notification.
#[test]
fn shell_cancel_observes_cancellation_before_wait() {
    let cancel = ShellCancel::default();
    cancel.cancel();
    let watcher_cancel = cancel.clone();
    let (tx, rx) = mpsc::channel();
    let watcher = std::thread::spawn(move || {
        watcher_cancel.wait_until_requested_or_completed();
        tx.send(watcher_cancel.should_report_cancel())
            .expect("test receiver should stay alive");
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(1))
            .expect("cancellation watcher should wake"),
        "cancellation should be reported when no completion raced with it"
    );
    watcher.join().expect("cancellation watcher");
}

/// Ensures completion wakes a blocked cancellation watcher so ordinary shell
/// completion cannot wedge while joining the watcher thread.
#[test]
fn shell_cancel_completion_wakes_waiter() {
    let cancel = ShellCancel::default();
    let watcher_cancel = cancel.clone();
    let (tx, rx) = mpsc::channel();
    let watcher = std::thread::spawn(move || {
        watcher_cancel.wait_until_requested_or_completed();
        tx.send(watcher_cancel.should_report_cancel())
            .expect("test receiver should stay alive");
    });
    cancel.mark_completed();
    assert!(
        !rx.recv_timeout(Duration::from_secs(1))
            .expect("completion should wake cancellation watcher"),
        "completed processes should not report cancellation"
    );
    watcher.join().expect("cancellation watcher");
}

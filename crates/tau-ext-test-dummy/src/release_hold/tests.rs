//! Focused release-frame parser and elapsed-budget tests.

use std::cell::{Cell, RefCell};
use std::io::{Cursor, Error, Read, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::{RELEASE_FRAME_MAX_BYTES, read_release_frame_with};

/// Reads fixture bytes with a budget that does not advance.
fn read_fixture(bytes: &[u8]) -> Option<super::ReleaseFrame> {
    read_release_frame_with(
        &mut Cursor::new(bytes),
        Duration::from_secs(1),
        || Duration::ZERO,
        |_, _| Ok(()),
    )
    .expect("configure fixture timeout")
}

/// Deterministic reader that advances elapsed time after each successful read.
struct AdvancingReader {
    /// Fixture bytes returned to the parser.
    bytes: Cursor<Vec<u8>>,
    /// Shared elapsed duration observed by the injected clock.
    elapsed: Rc<Cell<Duration>>,
    /// Elapsed time charged for each successful read.
    step: Duration,
}

impl Read for AdvancingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.bytes.read(buffer)?;
        if read != 0 {
            self.elapsed.set(self.elapsed.get() + self.step);
        }
        Ok(read)
    }
}

/// Deterministic reader that advances elapsed time when it returns the frame
/// delimiter.
struct DelimiterAdvancingReader {
    /// Fixture bytes returned to the parser.
    bytes: Cursor<Vec<u8>>,
    /// Shared elapsed duration observed by the injected clock.
    elapsed: Rc<Cell<Duration>>,
    /// Elapsed time reported after the delimiter is read.
    completion_elapsed: Duration,
}

impl Read for DelimiterAdvancingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.bytes.read(buffer)?;
        if read != 0 && buffer[0] == b'\n' {
            self.elapsed.set(self.completion_elapsed);
        }
        Ok(read)
    }
}

/// A complete frame is accepted at its newline without waiting for EOF.
#[test]
fn release_frame_does_not_require_eof() {
    let (mut writer, mut reader) = UnixStream::pair().expect("socket pair");
    writer
        .write_all(b"{\"call_id\":\"call-1\",\"release_nonce\":\"nonce\"}\n")
        .expect("write frame");
    let started = Instant::now();
    let frame = read_release_frame_with(
        &mut reader,
        Duration::from_secs(1),
        || started.elapsed(),
        |stream, timeout| stream.set_read_timeout(Some(timeout)),
    )
    .expect("configure read timeout")
    .expect("parse before EOF");
    assert_eq!(frame.call_id.as_str(), "call-1");
    assert_eq!(frame.release_nonce, "nonce");
}

/// The exact 4,096-byte boundary authenticates while one extra byte is
/// rejected.
#[test]
fn release_frame_enforces_exact_byte_boundary() {
    let prefix = b"{\"call_id\":\"call-1\",\"release_nonce\":\"".len();
    let suffix = b"\"}\n".len();
    let expected_nonce = "x".repeat(RELEASE_FRAME_MAX_BYTES - prefix - suffix);
    let accepted = format!("{{\"call_id\":\"call-1\",\"release_nonce\":\"{expected_nonce}\"}}\n");
    let rejected = format!(
        "{{\"call_id\":\"call-1\",\"release_nonce\":\"{}x\"}}\n",
        expected_nonce
    );

    let parsed = read_fixture(accepted.as_bytes()).expect("exact limit parses");
    assert_eq!(parsed.call_id.as_str(), "call-1");
    assert_eq!(parsed.release_nonce, expected_nonce);
    assert!(read_fixture(rejected.as_bytes()).is_none());
}

/// Successful partial reads consume one shared budget rather than renewing the
/// full socket timeout after every byte.
#[test]
fn release_frame_partial_progress_does_not_renew_elapsed_budget() {
    let elapsed = Rc::new(Cell::new(Duration::ZERO));
    let configured = Rc::new(RefCell::new(Vec::new()));
    let mut reader = AdvancingReader {
        bytes: Cursor::new(b"    ".to_vec()),
        elapsed: Rc::clone(&elapsed),
        step: Duration::from_millis(40),
    };

    let frame = read_release_frame_with(
        &mut reader,
        Duration::from_millis(100),
        || elapsed.get(),
        |_, timeout| {
            configured.borrow_mut().push(timeout);
            Ok(())
        },
    )
    .expect("configure read timeout");

    assert!(frame.is_none());
    assert_eq!(reader.bytes.position(), 3);
    assert_eq!(
        *configured.borrow(),
        [
            Duration::from_millis(100),
            Duration::from_millis(60),
            Duration::from_millis(20),
        ]
    );
}

/// Valid completion just before the budget succeeds, while completion at or
/// after exhaustion is rejected.
#[test]
fn release_frame_enforces_budget_at_valid_completion() {
    let bytes = b"{\"call_id\":\"call-1\",\"release_nonce\":\"nonce\"}\n";
    for (completion_elapsed, expect_frame) in [
        (Duration::from_millis(99), true),
        (Duration::from_millis(100), false),
        (Duration::from_millis(101), false),
    ] {
        let elapsed = Rc::new(Cell::new(Duration::ZERO));
        let mut reader = DelimiterAdvancingReader {
            bytes: Cursor::new(bytes.to_vec()),
            elapsed: Rc::clone(&elapsed),
            completion_elapsed,
        };
        let frame = read_release_frame_with(
            &mut reader,
            Duration::from_millis(100),
            || elapsed.get(),
            |_, timeout| {
                assert!(!timeout.is_zero());
                Ok(())
            },
        )
        .expect("configure read timeout");

        assert_eq!(frame.is_some(), expect_frame, "{completion_elapsed:?}");
    }
}

/// An already exhausted budget rejects the client without attempting a read or
/// installing a zero socket timeout.
#[test]
fn release_frame_exhausted_budget_skips_timeout_and_read() {
    let mut reader = Cursor::new(b"\n");
    let configured = Cell::new(false);

    let frame = read_release_frame_with(
        &mut reader,
        Duration::from_millis(100),
        || Duration::from_millis(100),
        |_, _| {
            configured.set(true);
            Ok(())
        },
    )
    .expect("expired budget is an ordinary rejection");

    assert!(frame.is_none());
    assert!(!configured.get());
    assert_eq!(reader.position(), 0);
}

/// Timeout-update failures remain configuration errors for worker arbitration
/// instead of becoming ordinary rejected frames.
#[test]
fn release_frame_exposes_timeout_configuration_failure() {
    let result = read_release_frame_with(
        &mut Cursor::new(b"\n"),
        Duration::from_millis(100),
        || Duration::ZERO,
        |_, _| Err(Error::other("injected timeout failure")),
    );
    let Err(error) = result else {
        panic!("timeout failure must remain visible");
    };

    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(error.to_string(), "injected timeout failure");
}

//! Focused release-frame parser boundary tests.

use std::io::{Cursor, Write};
use std::os::unix::net::UnixStream;

use super::{RELEASE_FRAME_MAX_BYTES, read_release_frame};

/// A complete frame is accepted at its newline without waiting for EOF.
#[test]
fn release_frame_does_not_require_eof() {
    let (mut writer, mut reader) = UnixStream::pair().expect("socket pair");
    writer
        .write_all(b"{\"call_id\":\"call-1\",\"release_nonce\":\"nonce\"}\n")
        .expect("write frame");
    let frame = read_release_frame(&mut reader).expect("parse before EOF");
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

    let parsed =
        read_release_frame(&mut Cursor::new(accepted.as_bytes())).expect("exact limit parses");
    assert_eq!(parsed.call_id.as_str(), "call-1");
    assert_eq!(parsed.release_nonce, expected_nonce);
    assert!(read_release_frame(&mut Cursor::new(rejected.as_bytes())).is_none());
}

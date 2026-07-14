use std::io::{Cursor, ErrorKind};

use super::*;

/// Fragmented reads, alternate ASCII casing, and optional whitespace must parse
/// exactly like an ordinary Content-Length request.
#[test]
fn bounded_reader_accepts_fragmented_case_insensitive_length() {
    /// Reader that fragments every source into single-byte chunks.
    struct OneByteReader {
        /// Buffered source bytes.
        inner: Cursor<Vec<u8>>,
    }
    impl Read for OneByteReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = buffer.len().min(1);
            self.inner.read(&mut buffer[..count])
        }
    }
    let bytes = b"POST /responses HTTP/1.1\r\ncOnTeNt-LeNgTh:\t 4 \t\r\n\r\nbody".to_vec();
    let request = read_bounded_http_request(&mut OneByteReader {
        inner: Cursor::new(bytes),
    })
    .expect("request");
    assert_eq!(request.request_line, b"POST /responses HTTP/1.1\r\n");
    assert_eq!(request.body, b"body");
}

/// Headers are bounded even when a peer never sends the terminator.
#[test]
fn bounded_reader_rejects_oversized_headers() {
    let mut bytes = vec![b'x'; MAX_FIXTURE_HEADERS];
    bytes.extend_from_slice(b"\r\n\r\n");
    assert_eq!(
        read_bounded_http_request(&mut Cursor::new(bytes))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
}

/// Oversized, duplicate, and invalid lengths fail before body allocation.
#[test]
fn bounded_reader_rejects_unsafe_content_lengths() {
    for headers in [
        format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_FIXTURE_BODY + 1
        ),
        "POST / HTTP/1.1\r\nContent-Length: 1\r\ncontent-length: 1\r\n\r\nx".to_owned(),
        "POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n".to_owned(),
    ] {
        assert_eq!(
            read_bounded_http_request(&mut Cursor::new(headers))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );
    }
}

/// EOF during either headers or the declared body fails promptly.
#[test]
fn bounded_reader_rejects_truncated_headers_and_body() {
    for bytes in [
        b"POST / HTTP/1.1\r\nContent-Length: 0\r\n".as_slice(),
        b"POST / HTTP/1.1\r\nContent-Length: 4\r\n\r\nxy".as_slice(),
    ] {
        assert_eq!(
            read_bounded_http_request(&mut Cursor::new(bytes))
                .unwrap_err()
                .kind(),
            ErrorKind::UnexpectedEof
        );
    }
}

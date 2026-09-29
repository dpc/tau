//! Shared helpers for length-prefixed durable record logs.

use std::io::{self, Read, Write};

use serde::Serialize;

/// Largest individual CBOR record that durable journal readers allocate.
///
/// A torn or corrupt length header can otherwise request an effectively
/// unbounded allocation. Writers use the same limit so every committed record
/// remains readable by the matching loader.
pub(crate) const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// Reads the next little-endian record length from a durable record log.
///
/// Clean EOF before a new 8-byte length header means the log ended normally and
/// returns `Ok(None)`. EOF after only part of the header is a torn write and
/// returns `UnexpectedEof` so replay fails closed instead of silently
/// truncating durable state.
pub(crate) fn read_record_length(reader: &mut impl Read) -> io::Result<Option<u64>> {
    let mut length_bytes = [0_u8; 8];
    let bytes_read = match reader.read(&mut length_bytes)? {
        0 => return Ok(None),
        bytes_read => bytes_read,
    };
    if bytes_read < length_bytes.len() {
        reader.read_exact(&mut length_bytes[bytes_read..])?;
    }
    Ok(Some(u64::from_le_bytes(length_bytes)))
}

pub(crate) fn encoded_size_with_limit<T: Serialize>(value: &T, limit: u64) -> Option<u64> {
    /// Non-retaining serialized-size counter.
    struct Counter {
        /// Bytes accepted so far.
        written: u64,
        /// Largest accepted total.
        limit: u64,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if self.written.saturating_add(length) > self.limit {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "encoded value exceeds bound",
                ));
            }
            self.written += length;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { written: 0, limit };
    tau_proto::encode_message(&mut counter, value)
        .ok()
        .map(|()| counter.written)
}

#[cfg(test)]
#[path = "record_log_tests.rs"]
mod tests;

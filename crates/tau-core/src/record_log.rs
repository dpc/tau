//! Shared helpers for length-prefixed durable record logs.

use std::io::{self, Read};

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

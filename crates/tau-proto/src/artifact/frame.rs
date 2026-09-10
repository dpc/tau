use std::io;

use serde::Serialize;

use super::ARTIFACT_FRAME_BYTES;

/// Measures the complete encoded frame using a bounded counting writer.
pub fn artifact_frame_fits(message: &impl Serialize) -> bool {
    crate::encode_message(Counter(0), message).is_ok()
}

/// Counts encoded bytes without allocating a duplicate transfer frame.
struct Counter(
    /// Total bytes emitted so far, saturated on overflow.
    usize,
);
impl std::io::Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > ARTIFACT_FRAME_BYTES {
            return Err(io::Error::other("artifact frame limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

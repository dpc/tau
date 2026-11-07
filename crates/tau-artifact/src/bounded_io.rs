//! Byte and cancellation budgets enforced during I/O, not only by metadata.

use std::io::{self, Read, Write};

use crate::{check_cancel, invalid};

/// I/O wrapper that refuses bytes beyond a budget and checks cancellation.
pub(crate) struct BoundedIo<T, C> {
    /// Owned stream or buffer.
    pub(crate) inner: T,
    /// Remaining bytes permitted through this stream.
    pub(crate) remaining: u64,
    /// Caller-owned cooperative cancellation check.
    pub(crate) cancelled: C,
}

impl<T: Read, C: Fn() -> bool> Read for BoundedIo<T, C> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        check_cancel(&self.cancelled)?;
        if buf.is_empty() {
            return Ok(0);
        }
        // Probe one byte at the exact boundary so an exhausted budget is not
        // EOF.
        let length = buf.len().min(self.remaining.saturating_add(1) as usize);
        let count = self.inner.read(&mut buf[..length])?;
        if count as u64 > self.remaining {
            return Err(invalid("directory stream exceeds byte limit"));
        }
        self.remaining -= count as u64;
        Ok(count)
    }
}

impl<T: Write, C: Fn() -> bool> Write for BoundedIo<T, C> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        check_cancel(&self.cancelled)?;
        if buf.len() as u64 > self.remaining {
            return Err(invalid("directory stream exceeds byte limit"));
        }
        let count = self.inner.write(buf)?;
        self.remaining -= count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        check_cancel(&self.cancelled)?;
        self.inner.flush()
    }
}

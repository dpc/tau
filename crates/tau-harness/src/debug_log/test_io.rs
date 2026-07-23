//! Deterministic file-I/O faults for [`super::DebugEventLog`] tests.

use std::io::{self, Seek, SeekFrom, Write};

use super::LineIo;

/// One deterministic failure plan for a test append.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct AppendFault {
    /// Line-byte offset that fails before that byte is written.
    pub(super) fail_write_at: Option<usize>,
    /// Whether the flush committing the complete line fails.
    pub(super) fail_commit_flush: bool,
    /// Whether rollback truncation fails.
    pub(super) fail_truncate: bool,
    /// Whether the rollback flush fails.
    pub(super) fail_rollback_flush: bool,
}

/// A real file wrapped with one deterministic line-append failure plan.
pub(super) struct FaultInjectingFile<'a> {
    /// Real debug log mutated by operations that the plan allows.
    file: &'a mut std::fs::File,
    /// Faults selected for this append.
    fault: AppendFault,
    /// Number of line bytes successfully written.
    written: usize,
    /// Complete line length.
    line_bytes: usize,
    /// Number of flush calls made by the append primitive.
    flush_calls: usize,
}

impl<'a> FaultInjectingFile<'a> {
    /// Wraps `file` with `fault`.
    pub(super) fn new(file: &'a mut std::fs::File, fault: AppendFault, line_bytes: usize) -> Self {
        Self {
            file,
            fault,
            written: 0,
            line_bytes,
            flush_calls: 0,
        }
    }
}

impl LineIo for FaultInjectingFile<'_> {
    fn seek_to_end(&mut self) -> io::Result<u64> {
        self.file.seek(SeekFrom::End(0))
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(fail_at) = self.fault.fail_write_at {
            if fail_at <= self.written {
                return Err(injected_error("line write"));
            }
            if fail_at < self.written.saturating_add(bytes.len()) {
                let accepted = fail_at - self.written;
                Write::write_all(self.file, &bytes[..accepted])?;
                self.written += accepted;
                return Err(injected_error("line write"));
            }
        }
        Write::write_all(self.file, bytes)?;
        self.written = self.written.saturating_add(bytes.len());
        Ok(())
    }

    fn truncate(&mut self, offset: u64) -> io::Result<()> {
        if self.fault.fail_truncate {
            Err(injected_error("rollback truncate"))
        } else {
            self.file.set_len(offset)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_calls = self.flush_calls.saturating_add(1);
        let is_commit_flush = self.flush_calls == 1 && self.written == self.line_bytes;
        if (is_commit_flush && self.fault.fail_commit_flush)
            || (!is_commit_flush && self.fault.fail_rollback_flush)
        {
            Err(injected_error("flush"))
        } else {
            Write::flush(self.file)
        }
    }
}

/// Creates a stable injected I/O error for assertions.
fn injected_error(operation: &str) -> io::Error {
    io::Error::other(format!("injected {operation} failure"))
}

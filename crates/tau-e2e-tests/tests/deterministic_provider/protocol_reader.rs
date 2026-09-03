//! Absolute-deadline, bounded reads for the persistence barrier protocol.

use std::fmt;
use std::io::{Error as IoError, ErrorKind, Read as _};
use std::os::fd::{AsFd as _, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::Instant;

use rustix_v1::event::{PollFd, PollFlags, Timespec, poll};
use rustix_v1::io::Errno;

/// Maximum bytes in one newline-terminated protocol record.
pub(super) const MAX_PROTOCOL_LINE_BYTES: usize = 512;

/// Absolute-deadline readiness result for a socket and process pidfd.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Readiness {
    /// Socket input, hangup, or error is ready.
    Socket,
    /// The observed process exited.
    ProcessExit,
    /// Neither descriptor became ready before the deadline.
    DeadlineExpired,
}

/// Waits once per interrupt against one shared absolute deadline.
pub(super) fn wait_for_readiness(
    socket_fd: BorrowedFd<'_>,
    process_fd: BorrowedFd<'_>,
    deadline: Instant,
) -> Result<Readiness, IoError> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(Readiness::DeadlineExpired);
        };
        let timeout = Timespec::try_from(remaining).map_err(|error| {
            IoError::new(
                ErrorKind::InvalidInput,
                format!("poll timeout does not fit platform timespec: {error}"),
            )
        })?;
        let flags = PollFlags::IN | PollFlags::HUP | PollFlags::ERR;
        let mut descriptors = [
            PollFd::new(&process_fd, flags),
            PollFd::new(&socket_fd, flags),
        ];
        match poll(&mut descriptors, Some(&timeout)) {
            Ok(0) => return Ok(Readiness::DeadlineExpired),
            Ok(_) if descriptors[1].revents().intersects(flags) => return Ok(Readiness::Socket),
            Ok(_) if descriptors[0].revents().intersects(flags) => {
                return Ok(Readiness::ProcessExit);
            }
            Ok(_) => continue,
            Err(Errno::INTR) => continue,
            Err(error) => return Err(IoError::from(error)),
        }
    }
}

/// Absolute-deadline reader for the two bounded protocol records.
pub(super) struct ProtocolReader {
    /// Nonblocking producer stream.
    stream: UnixStream,
    /// Bytes already read beyond a prior newline.
    buffered: Vec<u8>,
}

impl ProtocolReader {
    /// Creates a reader with no retained protocol bytes.
    pub(super) fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buffered: Vec::new(),
        }
    }

    /// Reads one bounded UTF-8 line without extending the shared deadline.
    pub(super) fn read_line(
        &mut self,
        process_fd: BorrowedFd<'_>,
        deadline: Instant,
    ) -> Result<String, ProtocolReadFailure> {
        loop {
            if let Some(newline) = self.buffered.iter().position(|byte| *byte == b'\n') {
                if MAX_PROTOCOL_LINE_BYTES < newline {
                    return Err(ProtocolReadFailure::Oversized);
                }
                let mut record = self.buffered.drain(..=newline).collect::<Vec<_>>();
                record.pop();
                return String::from_utf8(record).map_err(|_| ProtocolReadFailure::InvalidUtf8);
            }
            if self.buffered.len() > MAX_PROTOCOL_LINE_BYTES {
                return Err(ProtocolReadFailure::Oversized);
            }
            match wait_for_readiness(self.stream.as_fd(), process_fd, deadline)
                .map_err(ProtocolReadFailure::Io)?
            {
                Readiness::ProcessExit => return Err(ProtocolReadFailure::ProcessExit),
                Readiness::DeadlineExpired => return Err(ProtocolReadFailure::DeadlineExpired),
                Readiness::Socket => {}
            }
            let remaining = MAX_PROTOCOL_LINE_BYTES + 1 - self.buffered.len();
            let mut chunk = [0_u8; 128];
            let read_capacity = remaining.min(chunk.len());
            match self.stream.read(&mut chunk[..read_capacity]) {
                Ok(0) => {
                    if wait_for_process_exit(process_fd, deadline)
                        .map_err(ProtocolReadFailure::Io)?
                    {
                        return Err(ProtocolReadFailure::ProcessExit);
                    }
                    return Err(ProtocolReadFailure::UnexpectedEof);
                }
                Ok(bytes) => self.buffered.extend_from_slice(&chunk[..bytes]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                Err(error) => return Err(ProtocolReadFailure::Io(error)),
            }
        }
    }
}

/// Waits for process termination through the existing absolute protocol
/// deadline.
fn wait_for_process_exit(process_fd: BorrowedFd<'_>, deadline: Instant) -> Result<bool, IoError> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(false);
        };
        let timeout = Timespec::try_from(remaining).map_err(|error| {
            IoError::new(
                ErrorKind::InvalidInput,
                format!("poll timeout does not fit platform timespec: {error}"),
            )
        })?;
        let flags = PollFlags::IN | PollFlags::HUP | PollFlags::ERR;
        let mut descriptors = [PollFd::new(&process_fd, flags)];
        match poll(&mut descriptors, Some(&timeout)) {
            Ok(0) => return Ok(false),
            Ok(_) if descriptors[0].revents().intersects(flags) => return Ok(true),
            Ok(_) => continue,
            Err(Errno::INTR) => continue,
            Err(error) => return Err(IoError::from(error)),
        }
    }
}

/// Bounded protocol-record read failures.
#[derive(Debug)]
pub(super) enum ProtocolReadFailure {
    /// The observed daemon process exited.
    ProcessExit,
    /// The shared absolute deadline expired.
    DeadlineExpired,
    /// The producer closed an incomplete record.
    UnexpectedEof,
    /// A record exceeded the fixed byte cap.
    Oversized,
    /// A complete record was not UTF-8.
    InvalidUtf8,
    /// Polling or stream I/O failed.
    Io(std::io::Error),
}

impl fmt::Display for ProtocolReadFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProcessExit => formatter.write_str("daemon exited"),
            Self::DeadlineExpired => formatter.write_str("absolute deadline expired"),
            Self::UnexpectedEof => formatter.write_str("producer closed an incomplete record"),
            Self::Oversized => formatter.write_str("producer record exceeded 512 bytes"),
            Self::InvalidUtf8 => formatter.write_str("producer record was not UTF-8"),
            Self::Io(error) => write!(formatter, "I/O failed: {error}"),
        }
    }
}

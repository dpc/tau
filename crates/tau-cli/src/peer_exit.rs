//! Read-only confirmation of the exact attached daemon's process exit.

use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(all(test, target_os = "linux"))]
mod tests;

/// A process handle obtained from the connected socket, never from a reusable
/// PID or session pathname. Unsupported kernels remain explicitly unconfirmed.
pub(crate) struct PeerExit {
    /// Linux's socket-bound pidfd pins the peer incarnation even across PID
    /// reuse.
    #[cfg(target_os = "linux")]
    fd: OwnedFd,
}

impl PeerExit {
    /// Pin the peer of an admitted attached-UI socket without acquiring any
    /// signaling or session ownership.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)] // No socket-bound pidfd wrapper in our socket2 version.
    pub(crate) fn from_socket(stream: &UnixStream) -> io::Result<Self> {
        let mut fd: libc::c_int = -1;
        let mut len = std::mem::size_of_val(&fd) as libc::socklen_t;
        // SAFETY: both output pointers refer to initialized values of the exact
        // size supplied; the socket remains borrowed for the entire syscall.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&raw mut fd).cast(),
                &raw mut len,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful SO_PEERPIDFD returns a new owned, close-on-exec
        // fd.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Other platforms cannot currently confirm an unrelated daemon's exit.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn from_socket(_stream: &UnixStream) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "attached daemon exit confirmation is unavailable on this platform",
        ))
    }

    /// Wait boundedly for this exact process to exit; this makes no claim about
    /// successful persistence cleanup or a non-child's exit status.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)] // Read-only poll of an owned descriptor.
    pub(crate) fn wait(&self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut pollfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: pollfd is a live, initialized one-element array.
            let result = unsafe { libc::poll(&raw mut pollfd, 1, millis) };
            if 0 <= result {
                return Ok(pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            if deadline <= Instant::now() {
                return Ok(false);
            }
        }
    }

    /// Unsupported platforms conservatively leave the outcome unconfirmed.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn wait(&self, _timeout: Duration) -> io::Result<bool> {
        Ok(false)
    }
}

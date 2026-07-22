//! Shared pure process-group probes for core-resume process owners.

#![cfg(unix)]

use nix::sys::signal::kill;
use nix::unistd::Pid;

/// Returns whether any process still belongs to the supplied Unix process
/// group.
pub(super) fn exists(pgid: Pid) -> bool {
    match kill(Pid::from_raw(-pgid.as_raw()), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true,
    }
}

use std::os::fd::AsRawFd;
use std::process::Command;

use super::PtyStdio;

/// Ensures releasing output user guards after foreground exit exposes
/// immediate controller hangup instead of forcing a timed final drain.
#[test]
fn released_output_users_make_controller_terminally_ready() {
    let mut command = Command::new("sh");
    command.args(["-c", ":"]);
    let ptys = PtyStdio::attach(&mut command).expect("attach PTYs");
    let mut child = command.spawn().expect("spawn shell");
    assert!(child.wait().expect("wait shell").success());
    drop(command);

    let mut poll_fd = libc::pollfd {
        fd: ptys.stdout_controller.as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    drop(ptys.output_users);
    // SAFETY: `poll_fd` points to one initialized `pollfd`, its descriptor
    // remains owned by `stdout_controller`, and a zero timeout cannot block.
    #[allow(unsafe_code)]
    let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };

    assert_eq!(ready, 1);
    assert_ne!(poll_fd.revents & (libc::POLLHUP | libc::POLLERR), 0);
}

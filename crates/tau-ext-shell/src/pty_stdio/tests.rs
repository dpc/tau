use std::os::fd::AsRawFd;
use std::process::Command;

use super::{OutputModes, PtyStdio, open_pty, rustix};

/// Both endpoints returned by each platform allocator must be close-on-exec.
#[test]
fn allocated_endpoints_are_close_on_exec() {
    let pty = open_pty().expect("open PTY");
    for fd in [&pty.controller, &pty.user] {
        let flags = rustix::io::fcntl_getfd(fd).expect("inspect descriptor flags");
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }
}

/// Every parent-retained endpoint must be close-on-exec, including the guard
/// clones, so unrelated child processes cannot keep output PTYs alive.
#[test]
fn all_parent_retained_endpoints_are_close_on_exec() {
    let mut command = Command::new("sh");
    let ptys = PtyStdio::attach(&mut command).expect("attach PTYs");
    for fd in [
        &ptys.stdout_controller,
        &ptys.stderr_controller,
        &ptys.output_users[0],
        &ptys.output_users[1],
    ] {
        let flags = rustix::io::fcntl_getfd(fd).expect("inspect descriptor flags");
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }
}

/// Output PTYs keep the fixed geometry and byte-preserving output mode relied
/// on by shell capture semantics.
#[test]
fn allocated_pty_has_expected_terminal_configuration() {
    let pty = open_pty().expect("open PTY");
    let winsize = rustix::termios::tcgetwinsize(&pty.user).expect("read window size");
    let attributes = rustix::termios::tcgetattr(&pty.user).expect("read attributes");

    assert_eq!(winsize.ws_row, 24);
    assert_eq!(winsize.ws_col, 80);
    assert!(!attributes.output_modes.contains(OutputModes::OPOST));
}

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

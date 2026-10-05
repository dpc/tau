//! Isolated real-PTY oracles for crossterm's process-global input parser.

use std::fs::File;
use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use nix::fcntl::{FcntlArg, OFlag};

use super::*;

/// Private PTY peer with bounded waits and unconditional child cleanup.
struct PtyPeer {
    /// Isolated test process that exclusively owns the slave's input parser.
    child: Child,
    /// Application-facing input/output endpoint.
    master: File,
    /// Captured application output used for protocol ordering assertions.
    output: Vec<u8>,
}

impl PtyPeer {
    /// Starts the child in a separate controlling-terminal session.
    fn new(case: &str) -> Self {
        let pty = nix::pty::openpty(None, None).expect("private PTY");
        let slave = File::from(pty.slave);
        let master = File::from(pty.master);
        nix::fcntl::fcntl(master.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
            .expect("nonblocking PTY controller");
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "handoff_pty_tests::clipboard_handoff_pty_child",
                "--nocapture",
            ])
            .env("TAU_CLIPBOARD_HANDOFF_PTY_CHILD", case)
            .stdin(Stdio::from(slave.try_clone().expect("clone slave")))
            .stdout(Stdio::from(slave))
            .stderr(Stdio::piped());
        // SAFETY: this child-only hook uses async-signal-safe session/TTY
        // syscalls on the already-installed PTY stdin before exec.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(|| {
                if nix::libc::setsid() == -1 || nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: command.spawn().expect("spawn isolated child"),
            master,
            output: Vec::new(),
        }
    }

    /// Waits for an output marker, failing rather than hanging the test suite.
    fn wait_for(&mut self, marker: &[u8]) {
        let deadline = path_std_time::Instant::now() + Duration::from_secs(8);
        loop {
            let mut bytes = [0; 8192];
            match self.master.read(&mut bytes) {
                Ok(count) => self.output.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => {}
                Err(error) => panic!("PTY read: {error}"),
            }
            if self
                .output
                .windows(marker.len())
                .any(|bytes| bytes == marker)
            {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("poll child") {
                let mut stderr = String::new();
                self.child
                    .stderr
                    .as_mut()
                    .expect("piped child stderr")
                    .read_to_string(&mut stderr)
                    .expect("read child stderr");
                panic!("PTY child exited {status}: {stderr}");
            }
            assert!(
                path_std_time::Instant::now() < deadline,
                "PTY output marker timed out"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Delivers one small ordered terminal input write.
    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("write terminal input");
    }

    /// Extracts the unique metadata request without relying on payload logging.
    fn fence_id(&mut self) -> String {
        self.wait_for(b";Lg==\x1b\\");
        let output = String::from_utf8_lossy(&self.output);
        let start = output
            .find("\x1b]5522;type=read:id=tau-fence-")
            .expect("fresh metadata request");
        let id = output[start..]
            .split_once("id=")
            .expect("request has ID")
            .1
            .split(';')
            .next()
            .expect("request ID");
        id.to_owned()
    }

    /// Emits WezTerm's prompt-free metadata transcript, including header-only
    /// OK/DONE controls, never a content read.
    fn complete_fence(&mut self, id: &str) {
        self.send(
            format!(
                "\x1b]5522;type=read:id={id}:status=OK\x1b\\\
             \x1b]5522;type=read:id={id}:status=DATA:mime=Lg==;dGV4dC9wbGFpbg==\x1b\\\
             \x1b]5522;type=read:id={id}:status=DONE\x1b\\"
            )
            .as_bytes(),
        );
    }

    /// Requires the isolated child to finish successfully within a fixed wait.
    fn finish(&mut self) {
        let deadline = path_std_time::Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                let mut stderr = String::new();
                self.child
                    .stderr
                    .as_mut()
                    .expect("piped child stderr")
                    .read_to_string(&mut stderr)
                    .expect("read child stderr");
                assert!(status.success(), "PTY child failed {status}: {stderr}");
                return;
            }
            assert!(
                path_std_time::Instant::now() < deadline,
                "PTY child completion timed out"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for PtyPeer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Ctrl-O and a recognized partial OSC in the same OS input write cannot hand
/// the response suffix to a foreground callback, whether it precedes mode-off
/// or arrives afterwards. Queued complete old frames are drained as well.
#[test]
fn clipboard_handoff_real_pty_drains_recognized_partial_and_queued_frames() {
    for case in ["before-off", "after-off"] {
        let mut peer = PtyPeer::new(case);
        peer.wait_for(b"CHILD_READY");
        peer.send(b"\x0f\x1b]5522;type=read:id=old:status=DATA:mime=Lg==;d");
        peer.wait_for(b"CHILD_ACTION");
        if case == "before-off" {
            peer.send(b"GV4dC9wbGFpbg==\x1b\\xy\x1b]5522;type=read:id=old:status=DONE;\x1b\\");
        }
        let id = peer.fence_id();
        assert!(peer.output.windows(8).any(|bytes| bytes == b"\x1b[?5522l"));
        if case == "after-off" {
            peer.send(b"GV4dC9wbGFpbg==\x1b\\xy\x1b]5522;type=read:id=old:status=DONE;\x1b\\");
        }
        peer.complete_fence(&id);
        peer.wait_for(b"CHILD_RESUMED");
        peer.send(b"z");
        peer.finish();
    }
}

/// Missing DONE cancels rather than launching; late completion only drains
/// privately, and a new explicit Ctrl-O must obtain its own fresh fence.
#[test]
fn clipboard_handoff_real_pty_timeout_then_explicit_retry() {
    let mut peer = PtyPeer::new("retry");
    peer.wait_for(b"CHILD_READY");
    peer.send(b"\x0f\x1b]5522;type=read:id=old:status=OK;\x1b\\xy");
    let first = peer.fence_id();
    peer.send(format!("\x1b]5522;type=read:id={first}:status=OK;\x1b\\").as_bytes());
    peer.wait_for(b"CHILD_FAILED");
    peer.send(
        format!(
            "\x1b]5522;type=read:id={first}:status=DATA:mime=Lg==;\x1b\\\
                       \x1b]5522;type=read:id={first}:status=DONE;\x1b\\\x0f"
        )
        .as_bytes(),
    );
    // Find a second request rather than accepting the old output marker.
    peer.output.clear();
    let second = peer.fence_id();
    assert_ne!(first, second);
    peer.complete_fence(&second);
    peer.wait_for(b"CHILD_RESUMED");
    peer.send(b"z");
    peer.finish();
}

/// Unsupported terminals retain ordinary handoff without a dot-read response.
#[test]
fn clipboard_handoff_real_pty_unsupported_terminal_needs_no_fence() {
    let mut peer = PtyPeer::new("unsupported");
    peer.wait_for(b"CHILD_READY");
    peer.send(b"\x0f");
    peer.wait_for(b"CHILD_RESUMED");
    assert!(!peer.output.windows(10).any(|bytes| bytes == b"tau-fence-"));
    peer.send(b"z");
    peer.finish();
}

/// Normal interactive quit must accept the actual sender's header-only
/// controls while retaining the same complete correlated fence requirement.
#[test]
fn clipboard_handoff_real_pty_quit_accepts_wezterm_controls() {
    let mut peer = PtyPeer::new("quit");
    peer.wait_for(b"CHILD_READY");
    peer.send(b"\x0f");
    let id = peer.fence_id();
    peer.complete_fence(&id);
    peer.wait_for(b"CHILD_QUIT_PREPARED");
    peer.finish();
}

/// A correlated peer error or malformed DATA is immediate failure, not proof;
/// the raw terminal stays owned by Tau with its draft and staged keys intact.
#[test]
fn clipboard_handoff_real_pty_rejects_error_and_malformed_fence() {
    for case in ["error", "malformed"] {
        let mut peer = PtyPeer::new(case);
        peer.wait_for(b"CHILD_READY");
        peer.send(b"\x0f");
        let id = peer.fence_id();
        peer.send(b"xy");
        if case == "error" {
            peer.send(format!("\x1b]5522;type=read:id={id}:status=EPERM;\x1b\\").as_bytes());
        } else {
            peer.send(
                format!(
                    "\x1b]5522;type=read:id={id}:status=OK;\x1b\\\
                               \x1b]5522;type=read:id={id}:status=DATA:mime=Lg==;!\x1b\\"
                )
                .as_bytes(),
            );
        }
        peer.wait_for(b"CHILD_FAILED");
        peer.finish();
    }
}

/// Runs only in the isolated PTY subprocess; the ordinary suite never shares
/// its crossterm global input reader with this terminal.
#[test]
fn clipboard_handoff_pty_child() {
    let Ok(case) = std::env::var("TAU_CLIPBOARD_HANDOFF_PTY_CHILD") else {
        return;
    };
    let (term, handle) = Term::new("> ", TerminalOptions::default()).expect("real PTY terminal");
    handle.set_buffer("draft".into(), 5);
    if case != "unsupported" {
        let output = handle
            .lock()
            .editor
            .clipboard
            .mode_report(true, path_std_time::Instant::now())
            .output;
        term.write_clipboard_control(&output)
            .expect("enable native mode");
        assert!(handle.lock().editor.clipboard.needs_fence());
    }
    println!("CHILD_READY");
    io::stdout().flush().expect("publish ready marker");
    assert!(matches!(
        term.get_next_event().expect("editor request"),
        Event::ExternalEditor
    ));
    println!("CHILD_ACTION");
    io::stdout().flush().expect("publish action marker");
    if case == "before-off" {
        thread::sleep(Duration::from_millis(100));
    }
    if case == "quit" {
        term.prepare_interactive_exit()
            .expect("verified quit fence");
        assert!(handle.lock().terminal.exit_prepared);
        assert!(handle.lock().terminal.external_paused);
        assert!(!terminal::is_raw_mode_enabled().expect("query raw mode"));
        assert!(term.real_reader.borrow().is_none());
        assert!(!handle.lock().editor.clipboard.needs_fence());
        println!("CHILD_QUIT_PREPARED");
        io::stdout().flush().expect("publish quit marker");
        return;
    }
    if matches!(case.as_str(), "error" | "malformed") {
        assert!(term.pause_for_external().is_err());
        assert!(terminal::is_raw_mode_enabled().expect("query raw mode"));
        assert!(!handle.lock().terminal.external_paused);
        assert!(handle.lock().editor.clipboard.needs_fence());
        assert_eq!(handle.get_buffer(), "draft");
        for _ in 0..2 {
            assert!(matches!(
                term.get_next_event().expect("preserved ordinary key"),
                Event::BufferChanged
            ));
        }
        assert_eq!(handle.get_buffer(), "draftxy");
        println!("CHILD_FAILED");
        io::stdout().flush().expect("publish failure marker");
        return;
    }
    if case == "retry" {
        assert!(term.pause_for_external().is_err());
        assert!(!handle.lock().terminal.external_paused);
        assert_eq!(handle.get_buffer(), "draft");
        println!("CHILD_FAILED");
        io::stdout().flush().expect("publish failure marker");
        loop {
            if matches!(
                term.get_next_event().expect("explicit retry request"),
                Event::ExternalEditor
            ) {
                break;
            }
        }
        assert_eq!(handle.get_buffer(), "draftxy");
    }
    term.pause_for_external().expect("verified handoff");
    assert!(handle.lock().terminal.external_paused);
    assert!(!terminal::is_raw_mode_enabled().expect("query raw mode"));
    assert!(term.real_reader.borrow().is_none());
    assert!(!handle.lock().editor.clipboard.needs_fence());
    // The foreground callback is now the only input owner. No protocol suffix
    // or preserved key may be waiting on its kernel-facing stdin.
    // Reuse poll through a direct libc call without invoking crossterm again.
    let mut descriptor = nix::libc::pollfd {
        fd: 0,
        events: nix::libc::POLLIN,
        revents: 0,
    };
    // SAFETY: descriptor points to one initialized pollfd for this call.
    #[allow(unsafe_code)]
    let count = unsafe { nix::libc::poll(&mut descriptor, 1, 0) };
    assert_eq!(
        count, 0,
        "foreground callback must not inherit queued protocol/keys"
    );
    term.resume_after_external().expect("resume terminal");
    println!("CHILD_RESUMED");
    io::stdout().flush().expect("publish resume marker");
    loop {
        term.get_next_event().expect("ordinary resumed input");
        if handle.get_buffer().ends_with('z') {
            break;
        }
    }
    assert_eq!(
        handle.get_buffer(),
        if case == "unsupported" {
            "draftz"
        } else {
            "draftxyz"
        }
    );
}

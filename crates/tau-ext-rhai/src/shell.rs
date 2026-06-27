//! Direct trusted host shell execution for Rhai scripts.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

/// Maximum bytes captured per stdout or stderr pipe.
const MAX_CAPTURE_BYTES: usize = 512 * 1024;

/// Maximum time to drain immediately available pipe output after the foreground
/// shell command exits or is killed.
const POST_STOP_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);

/// Shared cancellation and process-group state for one shell worker.
#[derive(Clone, Default)]
pub(crate) struct ShellCancel {
    /// Whether the runtime has requested cancellation.
    requested: Arc<AtomicBool>,
    /// Unix process group id for direct shutdown kill, or zero before spawn.
    pgid: Arc<AtomicI32>,
}

impl ShellCancel {
    /// Request cancellation and kill the known process group when available.
    pub(crate) fn cancel(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.kill_process_group();
    }

    /// Check whether cancellation has been requested.
    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Record the shell child's process group id.
    fn set_process_group(&self, child: &std::process::Child) {
        #[cfg(unix)]
        self.pgid.store(child.id() as i32, Ordering::SeqCst);
    }

    /// Kill the recorded process group if this platform supports it.
    fn kill_process_group(&self) {
        #[cfg(unix)]
        {
            let pgid = self.pgid.load(Ordering::SeqCst);
            if pgid > 0 {
                kill_process_group_id(pgid);
            }
        }
    }
}

/// Run one shell command to completion with bounded capture and timeout.
pub(crate) fn run_shell_command(
    command: String,
    cwd: Option<String>,
    timeout: Duration,
    cancel: ShellCancel,
) -> serde_json::Value {
    let started = Instant::now();
    let mut command_builder = Command::new("sh");
    command_builder
        .arg("-c")
        .arg(&command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd.unwrap_or_else(|| ".".to_owned()));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        #[allow(unsafe_code)]
        // SAFETY: `pre_exec` runs in the child after fork and before exec. The
        // closure only calls the async-signal-safe `setsid` libc function and
        // constructs an OS error from errno on failure; it does not touch shared
        // Rust state from the parent process.
        unsafe {
            command_builder.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = match command_builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            return serde_json::json!({
                "success": false,
                "status": null,
                "signal": null,
                "timed_out": false,
                "termination_reason": "start_error",
                "output": format!("failed to start shell command: {error}"),
                "truncated": false,
                "valid_utf8": true,
            });
        }
    };
    cancel.set_process_group(&child);

    let pipe_stop = Arc::new(AtomicBool::new(false));
    let stdout = child
        .stdout
        .take()
        .map(|pipe| read_pipe_capped(pipe, pipe_stop.clone()));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| read_pipe_capped(pipe, pipe_stop.clone()));
    let deadline = Instant::now().checked_add(timeout);
    let mut timed_out = false;
    let mut canceled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if cancel.is_requested() => {
                canceled = true;
                kill_child_process_group(&mut child);
                break child.wait().ok();
            }
            Ok(None) if deadline.is_some_and(|deadline| Instant::now() < deadline) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                timed_out = true;
                kill_child_process_group(&mut child);
                break child.wait().ok();
            }
            Err(_) => break None,
        }
    };
    // A shell can exit while background descendants keep inherited stdout/stderr
    // pipes open. Kill the process group before joining pipe readers so
    // shutdown, timeout, and ordinary completion cannot wedge on normal
    // background descendants. Also ask pipe readers to stop after draining what
    // is immediately available: descendants may deliberately detach from the
    // process group while keeping inherited pipes open.
    cancel.kill_process_group();
    pipe_stop.store(true, Ordering::SeqCst);

    let out = stdout
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let err = stderr
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let mut bytes = out.bytes;
    let mut truncated = out.truncated;
    let mut valid_utf8 = out.valid_utf8;
    let mut output = out.text;
    if !err.text.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("[stderr]\n");
        output.push_str(&err.text);
    }
    bytes += err.bytes;
    truncated |= err.truncated;
    valid_utf8 &= err.valid_utf8;
    let total_lines = output.lines().count() as u64;
    let duration_seconds = started.elapsed().as_secs_f64();
    let status_code = status.as_ref().and_then(std::process::ExitStatus::code);
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.as_ref().and_then(|status| status.signal())
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let success = status
        .as_ref()
        .is_some_and(std::process::ExitStatus::success)
        && !timed_out;
    let termination_reason = if canceled {
        "canceled"
    } else if timed_out {
        "timeout"
    } else if signal.is_some() {
        "signal"
    } else if status.is_some() {
        "exit"
    } else {
        "wait_error"
    };
    serde_json::json!({
        "success": success,
        "status": status_code,
        "signal": signal,
        "timed_out": timed_out,
        "duration_seconds": duration_seconds,
        "termination_reason": termination_reason,
        "output": output,
        "truncated": truncated,
        "total_lines": if truncated { Some(total_lines) } else { None },
        "total_bytes": if truncated { Some(bytes as u64) } else { None },
        "valid_utf8": valid_utf8,
    })
}

fn kill_child_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        if kill_process_group_id(pid) == -1 {
            let _ = child.kill();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

#[cfg(unix)]
fn kill_process_group_id(pgid: i32) -> i32 {
    #[allow(unsafe_code)]
    // SAFETY: `pgid` is recorded from the child pid after `setsid`, so `-pgid`
    // targets that process group. Errors are intentionally ignored by callers
    // because the group may already have exited.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL)
    }
}

#[derive(Default)]
struct CapturedPipe {
    text: String,
    stored_bytes: usize,
    bytes: usize,
    truncated: bool,
    valid_utf8: bool,
}

impl CapturedPipe {
    fn new() -> Self {
        Self {
            valid_utf8: true,
            ..Default::default()
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        self.bytes += bytes.len();
        let room = MAX_CAPTURE_BYTES.saturating_sub(self.stored_bytes);
        if room < bytes.len() {
            self.truncated = true;
        }
        if room == 0 {
            return;
        }
        let take = room.min(bytes.len());
        self.stored_bytes += take;
        match std::str::from_utf8(&bytes[..take]) {
            Ok(s) => self.text.push_str(s),
            Err(_) => {
                self.valid_utf8 = false;
                self.text.push_str(&String::from_utf8_lossy(&bytes[..take]));
            }
        }
    }

    fn reached_capture_cap(&self) -> bool {
        MAX_CAPTURE_BYTES <= self.stored_bytes
    }
}

#[cfg(unix)]
fn read_pipe_capped<R>(mut pipe: R, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<CapturedPipe>
where
    R: Read + Send + 'static + std::os::fd::AsRawFd,
{
    set_nonblocking(&pipe);
    std::thread::spawn(move || {
        let mut captured = CapturedPipe::new();
        let mut buf = [0u8; 8192];
        let mut post_stop_deadline = None;
        loop {
            if post_stop_deadline.is_none() && stop.load(Ordering::SeqCst) {
                post_stop_deadline = Some(Instant::now() + POST_STOP_DRAIN_TIMEOUT);
            }
            if post_stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let sleep_for = post_stop_deadline
                        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                        .map(|remaining| remaining.min(Duration::from_millis(5)))
                        .unwrap_or_else(|| Duration::from_millis(5));
                    if sleep_for.is_zero() {
                        break;
                    }
                    std::thread::sleep(sleep_for);
                }
                Err(ref error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
                Ok(n) => {
                    captured.push_bytes(&buf[..n]);
                    if post_stop_deadline.is_some() && captured.reached_capture_cap() {
                        break;
                    }
                }
            }
        }
        captured
    })
}

#[cfg(not(unix))]
fn read_pipe_capped<R>(mut pipe: R, _stop: Arc<AtomicBool>) -> std::thread::JoinHandle<CapturedPipe>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut captured = CapturedPipe::new();
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => captured.push_bytes(&buf[..n]),
            }
        }
        captured
    })
}

#[cfg(unix)]
fn set_nonblocking<R: std::os::fd::AsRawFd>(pipe: &R) {
    let fd = pipe.as_raw_fd();
    #[allow(unsafe_code)]
    // SAFETY: `fd` is a live pipe descriptor borrowed from `pipe`. `fcntl` does
    // not take ownership of it. Failures only reduce us to ordinary blocking
    // pipe behavior for unusual platforms/descriptors, so they are ignored.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags != -1 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

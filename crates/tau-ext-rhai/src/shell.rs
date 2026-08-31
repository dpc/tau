//! Direct trusted host shell execution for Rhai scripts.

use std::{io as path_std_io, os as path_std_os, process as path_std_process};

#[cfg(test)]
mod tests;

use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Maximum bytes captured per stdout or stderr pipe.
const MAX_CAPTURE_BYTES: usize = 512 * 1024;

/// Maximum time to drain immediately available pipe output after the foreground
/// shell command exits or is killed.
const POST_STOP_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);

/// Shared cancellation and process-group state for one shell worker.
#[derive(Clone, Default)]
pub(crate) struct ShellCancel {
    /// Cancellation state guarded by the same mutex as its condition.
    inner: Arc<ShellCancelInner>,
    /// Unix process group id for direct shutdown kill, or zero before spawn.
    pgid: Arc<AtomicI32>,
}

impl ShellCancel {
    /// Request cancellation and kill the known process group when available.
    pub(crate) fn cancel(&self) {
        {
            let mut state = self.inner.lock_state();
            state.requested = true;
        }
        self.inner.changed.notify_all();
        self.kill_process_group();
    }

    /// Record that the child process has been reaped and wake waiters.
    fn mark_completed(&self) {
        {
            let mut state = self.inner.lock_state();
            state.completed = true;
        }
        self.inner.changed.notify_all();
    }

    /// Block until cancellation is requested or the child process exits.
    fn wait_until_requested_or_completed(&self) {
        let guard = self.inner.lock_state();
        drop(
            self.inner
                .changed
                .wait_while(guard, |state| !state.requested && !state.completed)
                .unwrap_or_else(|poison| poison.into_inner()),
        );
    }

    /// Check whether cancellation still needs to be reported to the worker.
    fn should_report_cancel(&self) -> bool {
        let state = self.inner.lock_state();
        state.requested && !state.completed
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
            if 0 < pgid {
                kill_process_group_id(pgid);
            }
        }
    }
}

/// Mutex-protected cancellation state plus its wake condition.
#[derive(Default)]
struct ShellCancelInner {
    /// State observed by cancellation watcher threads.
    state: Mutex<ShellCancelState>,
    /// Condition notified for every state transition.
    changed: Condvar,
}

impl ShellCancelInner {
    /// Lock cancellation state while tolerating poisoned mutexes during unwind.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, ShellCancelState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// State protected by the same mutex as `ShellCancelInner::changed`.
#[derive(Default)]
struct ShellCancelState {
    /// Whether the runtime has requested cancellation.
    requested: bool,
    /// Whether the shell worker has reaped the child process.
    completed: bool,
}

/// Run one shell command to completion with bounded capture and timeout.
pub(crate) fn run_shell_command(
    command: String,
    cwd: Option<PathBuf>,
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
        .current_dir(cwd.unwrap_or_else(|| PathBuf::from(".")));
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
                    return Err(path_std_io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let stdout_stop = match PipeStop::new() {
        Ok(stop) => stop,
        Err(error) => {
            return shell_start_error(format!("failed to prepare stdout wake pipe: {error}"));
        }
    };
    let stderr_stop = match PipeStop::new() {
        Ok(stop) => stop,
        Err(error) => {
            return shell_start_error(format!("failed to prepare stderr wake pipe: {error}"));
        }
    };

    let mut child = match command_builder.spawn() {
        Ok(child) => child,
        Err(error) => return shell_start_error(format!("failed to start shell command: {error}")),
    };
    cancel.set_process_group(&child);

    let stdout = child
        .stdout
        .take()
        .map(|pipe| read_pipe_capped(pipe, stdout_stop.clone()));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| read_pipe_capped(pipe, stderr_stop.clone()));
    let deadline = Instant::now().checked_add(timeout);
    let (process_tx, process_rx) = mpsc::channel();
    let waiter_tx = process_tx.clone();
    let watcher_cancel = cancel.clone();
    let waiter_cancel = cancel.clone();
    let process_cancel = cancel.clone();
    let cancel_watcher = std::thread::spawn(move || {
        watcher_cancel.wait_until_requested_or_completed();
        if watcher_cancel.should_report_cancel() {
            watcher_cancel.kill_process_group();
            let _ = process_tx.send(ProcessEvent::Canceled);
        }
    });
    let child_waiter = std::thread::spawn(move || {
        let status = child.wait().ok();
        waiter_cancel.mark_completed();
        let _ = waiter_tx.send(ProcessEvent::Exited(status));
    });
    let process_outcome = wait_for_process_event(&process_rx, deadline, &process_cancel);
    let _ = child_waiter.join();
    let _ = cancel_watcher.join();
    // A shell can exit while background descendants keep inherited stdout/stderr
    // pipes open. Kill the process group before joining pipe readers so
    // shutdown, timeout, and ordinary completion cannot wedge on normal
    // background descendants. Also ask pipe readers to stop after draining what
    // is immediately available: descendants may deliberately detach from the
    // process group while keeping inherited pipes open.
    process_cancel.kill_process_group();
    stdout_stop.request();
    stderr_stop.request();

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
    let status_code = process_outcome
        .status
        .as_ref()
        .and_then(path_std_process::ExitStatus::code);
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        process_outcome
            .status
            .as_ref()
            .and_then(|status| status.signal())
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let success = process_outcome
        .status
        .as_ref()
        .is_some_and(path_std_process::ExitStatus::success)
        && !process_outcome.timed_out;
    let termination_reason = if process_outcome.canceled {
        "canceled"
    } else if process_outcome.timed_out {
        "timeout"
    } else if signal.is_some() {
        "signal"
    } else if process_outcome.status.is_some() {
        "exit"
    } else {
        "wait_error"
    };
    serde_json::json!({
        "success": success,
        "status": status_code,
        "signal": signal,
        "timed_out": process_outcome.timed_out,
        "duration_seconds": duration_seconds,
        "termination_reason": termination_reason,
        "output": output,
        "truncated": truncated,
        "total_lines": if truncated { Some(total_lines) } else { None },
        "total_bytes": if truncated { Some(bytes as u64) } else { None },
        "valid_utf8": valid_utf8,
    })
}

/// Build a shell result for errors that happen before a shell child starts.
fn shell_start_error(output: String) -> serde_json::Value {
    serde_json::json!({
        "success": false,
        "status": null,
        "signal": null,
        "timed_out": false,
        "termination_reason": "start_error",
        "output": output,
        "truncated": false,
        "valid_utf8": true,
    })
}

/// Event sent by process supervision helper threads.
enum ProcessEvent {
    /// The shell process has exited and been reaped.
    Exited(Option<std::process::ExitStatus>),
    /// Runtime cancellation was requested before the shell exited.
    Canceled,
}

/// Outcome of waiting for shell process supervision events.
struct ProcessOutcome {
    /// Final process status, if the child waiter could reap it.
    status: Option<std::process::ExitStatus>,
    /// Whether the process exceeded the configured timeout.
    timed_out: bool,
    /// Whether runtime cancellation stopped the process.
    canceled: bool,
}

/// Wait for shell exit, cancellation, or timeout without polling the child.
fn wait_for_process_event(
    process_rx: &mpsc::Receiver<ProcessEvent>,
    deadline: Option<Instant>,
    cancel: &ShellCancel,
) -> ProcessOutcome {
    let event = match deadline {
        Some(deadline) => {
            let now = Instant::now();
            if deadline <= now {
                cancel.kill_process_group();
                return ProcessOutcome {
                    status: wait_for_exit_after_stop(process_rx),
                    timed_out: true,
                    canceled: false,
                };
            }
            process_rx.recv_timeout(deadline.saturating_duration_since(now))
        }
        None => process_rx
            .recv()
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
    };
    match event {
        Ok(ProcessEvent::Exited(status)) => ProcessOutcome {
            status,
            timed_out: false,
            canceled: false,
        },
        Ok(ProcessEvent::Canceled) => ProcessOutcome {
            status: wait_for_exit_after_stop(process_rx),
            timed_out: false,
            canceled: true,
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
            cancel.kill_process_group();
            ProcessOutcome {
                status: wait_for_exit_after_stop(process_rx),
                timed_out: true,
                canceled: false,
            }
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => ProcessOutcome {
            status: None,
            timed_out: false,
            canceled: false,
        },
    }
}

/// Wait for the child waiter to report process exit after a stop request.
fn wait_for_exit_after_stop(
    process_rx: &mpsc::Receiver<ProcessEvent>,
) -> Option<std::process::ExitStatus> {
    loop {
        match process_rx.recv() {
            Ok(ProcessEvent::Exited(status)) => return status,
            Ok(ProcessEvent::Canceled) => {}
            Err(_) => return None,
        }
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

/// Wakeable stop flag for pipe readers.
#[cfg(unix)]
struct PipeStop {
    /// Whether the owning shell worker requested post-stop draining.
    requested: AtomicBool,
    /// Read side used to wake `poll`.
    wake_read: OwnedFd,
    /// Write side used to wake `poll`.
    wake_write: OwnedFd,
}

#[cfg(unix)]
impl PipeStop {
    /// Create a new wakeable stop flag.
    fn new() -> io::Result<Arc<Self>> {
        let mut fds = [0, 0];
        #[allow(unsafe_code)]
        // SAFETY: `fds` points to two valid integers for libc to fill. On
        // success, returned descriptors are immediately wrapped in `OwnedFd`.
        let pipe_result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if pipe_result == -1 {
            return Err(io::Error::last_os_error());
        }
        #[allow(unsafe_code)]
        // SAFETY: `pipe` returned these descriptors to this function, so taking
        // ownership with `OwnedFd` is correct and ensures they close on drop.
        let wake_read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        #[allow(unsafe_code)]
        // SAFETY: `pipe` returned these descriptors to this function, so taking
        // ownership with `OwnedFd` is correct and ensures they close on drop.
        let wake_write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        configure_wake_fd(&wake_read)?;
        configure_wake_fd(&wake_write)?;
        Ok(Arc::new(Self {
            requested: AtomicBool::new(false),
            wake_read,
            wake_write,
        }))
    }

    /// Request post-stop draining and wake a blocked reader.
    fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
        let byte = [1u8];
        #[allow(unsafe_code)]
        // SAFETY: `wake_write` is owned by this `PipeStop`. The nonblocking
        // write is best-effort and intentionally ignores closed-pipe/full-pipe
        // errors because the flag itself carries the state.
        unsafe {
            let _ = libc::write(
                self.wake_write.as_raw_fd(),
                byte.as_ptr().cast(),
                byte.len(),
            );
        }
    }

    /// Check whether post-stop draining has been requested.
    fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

#[cfg(unix)]
fn configure_wake_fd(fd: &OwnedFd) -> io::Result<()> {
    let raw_fd = fd.as_raw_fd();
    #[allow(unsafe_code)]
    // SAFETY: `raw_fd` is a live descriptor borrowed from `fd`. `fcntl` does not
    // take ownership of it. Errors are reported so callers can fail before
    // spawning a shell whose pipe reader could not be woken.
    unsafe {
        let flags = libc::fcntl(raw_fd, libc::F_GETFL);
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(io::Error::last_os_error());
        }
        let fd_flags = libc::fcntl(raw_fd, libc::F_GETFD);
        if fd_flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(raw_fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Wakeable stop flag for pipe readers on non-Unix platforms.
#[cfg(not(unix))]
struct PipeStop {
    /// Whether the owning shell worker requested post-stop draining.
    requested: AtomicBool,
}

#[cfg(not(unix))]
impl PipeStop {
    /// Create a new stop flag.
    fn new() -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            requested: AtomicBool::new(false),
        }))
    }

    /// Request post-stop draining.
    fn request(&self) {
        self.requested.store(true, Ordering::SeqCst);
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
fn read_pipe_capped<R>(mut pipe: R, stop: Arc<PipeStop>) -> std::thread::JoinHandle<CapturedPipe>
where
    R: Read + Send + 'static + path_std_os::fd::AsRawFd,
{
    set_nonblocking(&pipe);
    let pipe_fd = pipe.as_raw_fd();
    std::thread::spawn(move || {
        let mut captured = CapturedPipe::new();
        let mut buf = [0u8; 8192];
        let mut post_stop_deadline = None;
        loop {
            if post_stop_deadline.is_none() && stop.is_requested() {
                post_stop_deadline = Some(Instant::now() + POST_STOP_DRAIN_TIMEOUT);
            }
            if post_stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Err(ref error) if error.kind() == path_std_io::ErrorKind::WouldBlock => {
                    let timeout = post_stop_deadline
                        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                        .map(duration_to_poll_timeout_ms)
                        .unwrap_or(-1);
                    if timeout == 0 {
                        break;
                    }
                    let wake_fd = post_stop_deadline.is_none().then(|| stop.wake_read.as_fd());
                    wait_for_pipe_readiness(pipe_fd, wake_fd, timeout);
                }
                Err(ref error) if error.kind() == path_std_io::ErrorKind::Interrupted => {}
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
fn read_pipe_capped<R>(mut pipe: R, _stop: Arc<PipeStop>) -> std::thread::JoinHandle<CapturedPipe>
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
fn duration_to_poll_timeout_ms(duration: Duration) -> i32 {
    if duration.is_zero() {
        return 0;
    }
    let millis = duration.as_millis();
    if millis == 0 {
        1
    } else {
        millis.min(i32::MAX as u128) as i32
    }
}

#[cfg(unix)]
fn wait_for_pipe_readiness(pipe_fd: RawFd, wake_fd: Option<BorrowedFd<'_>>, timeout_ms: i32) {
    let wake_fd = wake_fd.map(|fd| fd.as_raw_fd());
    let mut fds = [
        libc::pollfd {
            fd: pipe_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_fd.unwrap_or(pipe_fd),
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
    ];
    let nfds = if wake_fd.is_some() { 2 } else { 1 };
    #[allow(unsafe_code)]
    // SAFETY: `fds` contains borrowed file descriptors that remain valid for
    // the duration of the call. `poll` does not take ownership of them.
    unsafe {
        let _ = libc::poll(fds.as_mut_ptr(), nfds, timeout_ms);
    }
}

#[cfg(unix)]
fn set_nonblocking<R: path_std_os::fd::AsRawFd>(pipe: &R) {
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

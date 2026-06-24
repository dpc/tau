//! Bounded subprocess execution for CLI prompt/completion commands.
//!
//! This module centralizes stdout limits, elapsed timeouts, inherited-pipe
//! handling, process-group cleanup, and foreground-terminal ownership for
//! interactive prompt/completion commands.

#[cfg(test)]
mod tests;

use std::io::{self, Read, Write as _};
#[cfg(unix)]
use std::os::fd::AsFd as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const POST_EXIT_PIPE_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(test)]
static FAIL_NEXT_FOREGROUND_CLAIM: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static FAIL_FOREGROUND_CLAIM_FOR_CHILD_ID: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
static LAST_FAILED_FOREGROUND_CHILD_ID: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
static FOREGROUND_CLAIM_TEST_LOCK: Mutex<()> = Mutex::new(());

/// How much subprocess ownership the bounded runner should take on failures.
///
/// Use [`ProcessOwnership::ProcessGroup`] for bounded non-interactive
/// completion/git helpers: a separate process group lets Tau kill descendants
/// that inherit stdout without changing foreground terminal ownership. Use
/// [`ProcessOwnership::ForegroundProcessGroup`] for terminal-releasing prompt
/// shell actions, where interactive descendants also need temporary foreground
/// control of the terminal.
#[derive(Clone, Copy)]
pub(crate) enum ProcessOwnership {
    /// Put the child in a new process group and terminate that group on errors,
    /// without changing foreground terminal ownership.
    ProcessGroup,
    /// Put the child in a new process group, hand it foreground terminal
    /// ownership, and terminate that group on errors.
    ForegroundProcessGroup,
}

/// Captured output and exit status from a bounded subprocess.
#[derive(Debug)]
pub(crate) struct BoundedCommandOutput {
    /// Direct child exit status.
    pub(crate) status: std::process::ExitStatus,
    /// Captured stdout, guaranteed to be at most the configured byte limit.
    pub(crate) stdout: Vec<u8>,
}

/// Exit status from a bounded subprocess whose stdio is owned by the caller.
#[derive(Debug)]
pub(crate) struct BoundedCommandStatus {
    /// Direct child exit status.
    pub(crate) status: std::process::ExitStatus,
}

/// Runs a child process while bounding captured stdout and elapsed time.
///
/// Callers must configure stderr before calling. This helper always captures
/// stdout and sets stdin to a pipe when `stdin_input` is present; otherwise
/// callers should configure stdin explicitly (usually `Stdio::null()`). Stdout
/// is drained on a background thread before any optional stdin write is
/// attempted, so a child that writes before reading cannot deadlock the prompt.
/// If stdout crosses `stdout_limit`, if stdin writing fails for anything other
/// than `BrokenPipe`, if the direct child exceeds `timeout`, or if inherited
/// pipes do not close shortly after the direct child exits, the child or owned
/// process group is killed/reaped when possible and an error is returned.
pub(crate) fn run_with_bounded_stdout(
    command: &mut std::process::Command,
    stdin_input: Option<&[u8]>,
    stdout_limit: usize,
    timeout: std::time::Duration,
    ownership: ProcessOwnership,
) -> Result<BoundedCommandOutput, String> {
    configure_process_ownership(command, ownership)?;
    if stdin_input.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not spawn command: {e}"))?;
    let process_group = match claim_process_group_handle(ownership, child.id()) {
        Ok(process_group) => process_group,
        Err(error) => {
            #[cfg(test)]
            LAST_FAILED_FOREGROUND_CHILD_ID.store(child.id(), Ordering::SeqCst);
            let child_pgid = match ownership {
                ProcessOwnership::ProcessGroup | ProcessOwnership::ForegroundProcessGroup => {
                    process_group_id(child.id())
                }
            };
            terminate_child(&mut child, child_pgid);
            return Err(error);
        }
    };

    BoundedStdoutRun::start(child, process_group, stdin_input, stdout_limit)?.finish(timeout)
}

/// State for one bounded stdout child run.
struct BoundedStdoutRun {
    /// Spawned direct child being monitored.
    child: std::process::Child,
    /// Owned process-group/foreground-terminal handle for the child.
    process_group: ProcessGroupHandle,
    /// Reader-thread result channel for captured stdout.
    stdout_rx: std::sync::mpsc::Receiver<io::Result<LimitedRead>>,
    /// Optional writer-thread result channel for piped stdin.
    stdin_rx: Option<std::sync::mpsc::Receiver<io::Result<()>>>,
    /// Completed stdout read observed before the direct child exited.
    stdout_result: Option<io::Result<LimitedRead>>,
    /// Whether the optional stdin writer has completed.
    stdin_done: bool,
    /// Maximum number of stdout bytes allowed before terminating the child.
    stdout_limit: usize,
}

impl BoundedStdoutRun {
    /// Starts background pipe handling for a spawned child.
    fn start(
        mut child: std::process::Child,
        process_group: ProcessGroupHandle,
        stdin_input: Option<&[u8]>,
        stdout_limit: usize,
    ) -> Result<Self, String> {
        let Some(stdout) = child.stdout.take() else {
            terminate_child(&mut child, process_group.child_pgid());
            return Err("command stdout was not captured".to_owned());
        };
        let stdout_rx = spawn_stdout_reader(stdout, stdout_limit);
        let stdin_rx = stdin_input.and_then(|input| spawn_stdin_writer(&mut child, input));
        let stdin_done = stdin_rx.is_none();
        Ok(Self {
            child,
            process_group,
            stdout_rx,
            stdin_rx,
            stdout_result: None,
            stdin_done,
            stdout_limit,
        })
    }

    /// Polls the child until it exits, a pipe error occurs, or the timeout
    /// expires.
    fn finish(mut self, timeout: std::time::Duration) -> Result<BoundedCommandOutput, String> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() >= deadline {
                self.terminate();
                self.wait_for_stdin_writer();
                return Err(format!("command exceeded {}s timeout", timeout.as_secs()));
            }

            self.poll_stdout()?;
            self.poll_stdin()?;
            if let Some(output) = self.poll_child()? {
                return Ok(output);
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Incorporates any completed stdout read, terminating on reader failures.
    fn poll_stdout(&mut self) -> Result<(), String> {
        if self.stdout_result.is_some() {
            return Ok(());
        }
        match self.stdout_rx.try_recv() {
            Ok(Ok(stdout)) if stdout.overflowed => {
                self.terminate();
                self.wait_for_stdin_writer();
                Err(format!(
                    "command stdout exceeded {} bytes",
                    self.stdout_limit
                ))
            }
            Ok(Err(error)) => {
                self.terminate();
                self.wait_for_stdin_writer();
                Err(format!("could not read command stdout: {error}"))
            }
            Ok(result) => {
                self.stdout_result = Some(result);
                Ok(())
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(()),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.terminate();
                self.wait_for_stdin_writer();
                Err("command stdout reader stopped unexpectedly".to_owned())
            }
        }
    }

    /// Incorporates any completed stdin write, terminating on writer failures.
    fn poll_stdin(&mut self) -> Result<(), String> {
        if self.stdin_done {
            return Ok(());
        }
        let Some(rx) = self.stdin_rx.as_ref() else {
            self.stdin_done = true;
            return Ok(());
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.stdin_done = true;
                Ok(())
            }
            Ok(Err(error)) => {
                self.terminate();
                Err(format!("could not write to command stdin: {error}"))
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(()),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.stdin_done = true;
                Ok(())
            }
        }
    }

    /// Checks direct-child status and collects final pipe results after exit.
    fn poll_child(&mut self) -> Result<Option<BoundedCommandOutput>, String> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let stdout = match receive_stdout_after_child_exit(
                    self.stdout_result.take(),
                    &self.stdout_rx,
                    self.stdout_limit,
                ) {
                    Ok(stdout) => stdout,
                    Err(error) => {
                        self.terminate_process_group_if_owned();
                        return Err(error);
                    }
                };
                if let Err(error) = wait_for_stdin_after_child_exit(self.stdin_rx.as_ref()) {
                    self.terminate_process_group_if_owned();
                    return Err(error);
                }
                Ok(Some(BoundedCommandOutput {
                    status,
                    stdout: stdout.bytes,
                }))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.terminate();
                self.wait_for_stdin_writer();
                Err(format!("could not wait for command: {error}"))
            }
        }
    }

    /// Terminates the direct child or its process group and reaps it.
    fn terminate(&mut self) {
        terminate_child(&mut self.child, self.process_group.child_pgid());
    }

    /// Terminates the owned process group without waiting for the
    /// already-exited child.
    fn terminate_process_group_if_owned(&self) {
        terminate_process_group_if_owned(self.process_group.child_pgid());
    }

    /// Waits briefly for an in-flight stdin writer after child termination.
    fn wait_for_stdin_writer(&self) {
        wait_for_stdin_writer(self.stdin_rx.as_ref());
    }
}

fn spawn_stdout_reader(
    stdout: impl Read + Send + 'static,
    stdout_limit: usize,
) -> std::sync::mpsc::Receiver<io::Result<LimitedRead>> {
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = stdout_tx.send(read_to_limit(stdout, stdout_limit));
    });
    stdout_rx
}

fn spawn_stdin_writer(
    child: &mut std::process::Child,
    input: &[u8],
) -> Option<std::sync::mpsc::Receiver<io::Result<()>>> {
    let mut stdin = child.stdin.take()?;
    let input = input.to_vec();
    let (stdin_tx, stdin_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = match stdin.write_all(&input) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            Err(error) => Err(error),
        };
        let _ = stdin_tx.send(result);
    });
    Some(stdin_rx)
}

/// Bounded stdout read result from the reader thread.
struct LimitedRead {
    /// Captured bytes up to the configured limit.
    bytes: Vec<u8>,
    /// Whether the reader observed more bytes than the configured limit.
    overflowed: bool,
}

/// Runs a child process while bounding elapsed time without capturing output.
///
/// Callers must configure stdin/stdout/stderr before calling. This is intended
/// for terminal-releasing prompt edit actions where the child needs the real
/// terminal file descriptors rather than Tau-owned pipes. Process-group and
/// foreground ownership behavior matches [`run_with_bounded_stdout`].
pub(crate) fn run_with_inherited_stdio(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    ownership: ProcessOwnership,
) -> Result<BoundedCommandStatus, String> {
    configure_process_ownership(command, ownership)?;
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not spawn command: {e}"))?;
    let process_group = match claim_process_group_handle(ownership, child.id()) {
        Ok(process_group) => process_group,
        Err(error) => {
            #[cfg(test)]
            LAST_FAILED_FOREGROUND_CHILD_ID.store(child.id(), Ordering::SeqCst);
            let child_pgid = match ownership {
                ProcessOwnership::ProcessGroup | ProcessOwnership::ForegroundProcessGroup => {
                    process_group_id(child.id())
                }
            };
            terminate_child(&mut child, child_pgid);
            return Err(error);
        }
    };

    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() >= deadline {
            terminate_child(&mut child, process_group.child_pgid());
            return Err(format!("command exceeded {}s timeout", timeout.as_secs()));
        }

        match child.try_wait() {
            Ok(Some(status)) => return Ok(BoundedCommandStatus { status }),
            Ok(None) => {}
            Err(error) => {
                terminate_child(&mut child, process_group.child_pgid());
                return Err(format!("could not wait for command: {error}"));
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn read_to_limit(mut reader: impl Read, limit: usize) -> io::Result<LimitedRead> {
    let mut bytes = Vec::new();
    let mut buf = [0; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        if remaining < n {
            bytes.extend_from_slice(&buf[..remaining]);
            return Ok(LimitedRead {
                bytes,
                overflowed: true,
            });
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    Ok(LimitedRead {
        bytes,
        overflowed: false,
    })
}

fn receive_stdout_after_child_exit(
    stdout_result: Option<io::Result<LimitedRead>>,
    stdout_rx: &std::sync::mpsc::Receiver<io::Result<LimitedRead>>,
    stdout_limit: usize,
) -> Result<LimitedRead, String> {
    let stdout = match stdout_result {
        Some(result) => result,
        None => stdout_rx
            .recv_timeout(POST_EXIT_PIPE_CLOSE_TIMEOUT)
            .map_err(|_| "command stdout pipe did not close after child exit".to_owned())?,
    }
    .map_err(|e| format!("could not read command stdout: {e}"))?;
    if stdout.overflowed {
        return Err(format!("command stdout exceeded {} bytes", stdout_limit));
    }
    Ok(stdout)
}

fn wait_for_stdin_after_child_exit(
    rx: Option<&std::sync::mpsc::Receiver<io::Result<()>>>,
) -> Result<(), String> {
    let Some(rx) = rx else {
        return Ok(());
    };
    match rx.recv_timeout(POST_EXIT_PIPE_CLOSE_TIMEOUT) {
        Ok(Ok(())) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(()),
        Ok(Err(error)) => Err(format!("could not write to command stdin: {error}")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err("command stdin pipe did not close after child exit".to_owned())
        }
    }
}

fn wait_for_stdin_writer(rx: Option<&std::sync::mpsc::Receiver<io::Result<()>>>) {
    if let Some(rx) = rx {
        let _ = rx.recv_timeout(POST_EXIT_PIPE_CLOSE_TIMEOUT);
    }
}

/// Process group id assigned to a spawned child.
#[derive(Clone, Copy)]
struct ChildProcessGroupId(i32);

impl ChildProcessGroupId {
    /// Builds the child process group id from the direct child pid because
    /// owned subprocesses are spawned with `process_group(0)`.
    fn from_child_id(child_id: u32) -> Self {
        Self(child_id as i32)
    }

    #[cfg(unix)]
    fn as_nix_pid(self) -> nix::unistd::Pid {
        nix::unistd::Pid::from_raw(self.0)
    }
}

fn terminate_child(child: &mut std::process::Child, child_pgid: Option<ChildProcessGroupId>) {
    if let Some(child_pgid) = child_pgid {
        terminate_process_group(child_pgid);
    } else {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn terminate_process_group_if_owned(child_pgid: Option<ChildProcessGroupId>) {
    if let Some(child_pgid) = child_pgid {
        terminate_process_group(child_pgid);
    }
}

fn terminate_process_group(child_pgid: ChildProcessGroupId) {
    #[cfg(unix)]
    {
        let pgid = child_pgid.as_nix_pid();
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGTERM);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    }
}

fn configure_process_ownership(
    command: &mut std::process::Command,
    ownership: ProcessOwnership,
) -> Result<(), String> {
    match ownership {
        ProcessOwnership::ProcessGroup | ProcessOwnership::ForegroundProcessGroup => {
            configure_process_group(command)
        }
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut std::process::Command) -> Result<(), String> {
    command.process_group(0);
    Ok(())
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut std::process::Command) -> Result<(), String> {
    Ok(())
}

fn process_group_id(child_id: u32) -> Option<ChildProcessGroupId> {
    #[cfg(unix)]
    {
        Some(ChildProcessGroupId::from_child_id(child_id))
    }
    #[cfg(not(unix))]
    {
        let _ = child_id;
        None
    }
}

/// Foreground terminal/process-group state for an owned prompt action.
struct ProcessGroupHandle {
    /// Process group id assigned to the direct child and its descendants.
    child_pgid: Option<ChildProcessGroupId>,
    /// Tau's foreground process group, restored before the prompt resumes.
    #[cfg(unix)]
    parent_pgid: Option<nix::unistd::Pid>,
}

impl ProcessGroupHandle {
    fn new(ownership: ProcessOwnership, child_id: u32) -> Result<Self, String> {
        match ownership {
            ProcessOwnership::ProcessGroup => Ok(Self {
                child_pgid: process_group_id(child_id),
                #[cfg(unix)]
                parent_pgid: None,
            }),
            ProcessOwnership::ForegroundProcessGroup => Self::claim_foreground(child_id),
        }
    }

    fn child_pgid(&self) -> Option<ChildProcessGroupId> {
        self.child_pgid
    }

    #[cfg(unix)]
    fn claim_foreground(child_id: u32) -> Result<Self, String> {
        let parent_pgid =
            current_foreground_process_group().unwrap_or_else(|_| nix::unistd::getpgrp());
        let child_pgid = ChildProcessGroupId::from_child_id(child_id);
        set_foreground_process_group(child_pgid.as_nix_pid())
            .map_err(|e| format!("could not hand terminal to prompt action: {e}"))?;
        let _ =
            nix::sys::signal::killpg(child_pgid.as_nix_pid(), nix::sys::signal::Signal::SIGCONT);
        Ok(Self {
            child_pgid: Some(child_pgid),
            parent_pgid: Some(parent_pgid),
        })
    }

    #[cfg(not(unix))]
    fn claim_foreground(_child_id: u32) -> Result<Self, String> {
        Err("foreground process-group prompt actions are unsupported on this platform".to_owned())
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupHandle {
    fn drop(&mut self) {
        if let Some(parent_pgid) = self.parent_pgid {
            let _ = set_foreground_process_group(parent_pgid);
        }
    }
}

#[cfg(unix)]
fn current_foreground_process_group() -> nix::Result<nix::unistd::Pid> {
    with_controlling_terminal(|fd| nix::unistd::tcgetpgrp(fd))
}

#[cfg(unix)]
fn set_foreground_process_group(pgid: nix::unistd::Pid) -> nix::Result<()> {
    #[cfg(test)]
    {
        let target = FAIL_FOREGROUND_CLAIM_FOR_CHILD_ID.load(Ordering::SeqCst);
        if target != 0
            && pgid.as_raw() == target as i32
            && FAIL_FOREGROUND_CLAIM_FOR_CHILD_ID
                .compare_exchange(target, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(nix::errno::Errno::EIO);
        }
    }
    // In non-interactive tests or redirected invocations there may be no
    // controlling terminal; treat ENOTTY as a no-op so direct subprocess
    // lifecycle checks can still run. Prefer /dev/tty for real prompt actions,
    // but fall back to stdin for embeddings where stdin is the controlling tty.
    match with_controlling_terminal(|fd| tcsetpgrp_blocking_sigtou(fd, pgid)) {
        Ok(()) | Err(nix::errno::Errno::ENOTTY) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn with_controlling_terminal<T>(
    f: impl FnOnce(std::os::fd::BorrowedFd<'_>) -> nix::Result<T>,
) -> nix::Result<T> {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(tty) => f(tty.as_fd()),
        Err(_) => f(std::io::stdin().as_fd()),
    }
}

#[cfg(unix)]
fn tcsetpgrp_blocking_sigtou(
    fd: std::os::fd::BorrowedFd<'_>,
    pgid: nix::unistd::Pid,
) -> nix::Result<()> {
    let mut block = nix::sys::signal::SigSet::empty();
    block.add(nix::sys::signal::Signal::SIGTTOU);
    let mut previous = nix::sys::signal::SigSet::empty();
    nix::sys::signal::pthread_sigmask(
        nix::sys::signal::SigmaskHow::SIG_BLOCK,
        Some(&block),
        Some(&mut previous),
    )?;
    let result = nix::unistd::tcsetpgrp(fd, pgid);
    let restore = nix::sys::signal::pthread_sigmask(
        nix::sys::signal::SigmaskHow::SIG_SETMASK,
        Some(&previous),
        None,
    );
    result.and(restore)
}

fn claim_process_group_handle(
    ownership: ProcessOwnership,
    child_id: u32,
) -> Result<ProcessGroupHandle, String> {
    #[cfg(test)]
    if matches!(ownership, ProcessOwnership::ForegroundProcessGroup)
        && FAIL_NEXT_FOREGROUND_CLAIM.swap(false, Ordering::SeqCst)
    {
        FAIL_FOREGROUND_CLAIM_FOR_CHILD_ID.store(child_id, Ordering::SeqCst);
    }
    ProcessGroupHandle::new(ownership, child_id)
}

//! Bounded subprocess execution for CLI prompt/completion commands.
//!
//! This module centralizes stdout limits, elapsed timeouts, inherited-pipe
//! handling, process-group cleanup, and foreground-terminal ownership for
//! interactive prompt/completion commands.

use std::sync::mpsc as path_std_sync_mpsc;
use std::{
    fs as path_std_fs, os as path_std_os, process as path_std_process, sync as path_std_sync,
    time as path_std_time,
};

use nix::sys::signal as path_nix_sys_signal;
use nix::{errno as path_nix_errno, sys as path_nix_sys, unistd as path_nix_unistd};

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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

const POST_EXIT_PIPE_CLOSE_TIMEOUT: std::time::Duration = path_std_time::Duration::from_secs(1);
#[cfg(test)]
static FAIL_NEXT_FOREGROUND_CLAIM: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
thread_local! {
    /// Per-test-thread restoration failure injection.
    static FAIL_FOREGROUND_RESTORE_LOCAL: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}
#[cfg(test)]
pub(super) static FAIL_FOREGROUND_RESTORE: ForegroundRestoreFailureInjection =
    ForegroundRestoreFailureInjection;
#[cfg(test)]
static FOREGROUND_RESTORE_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static LAST_FAILED_FOREGROUND_CHILD_ID: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
pub(super) static FOREGROUND_CLAIM_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Test-only foreground-restoration failure switch isolated per test thread.
#[cfg(test)]
pub(super) struct ForegroundRestoreFailureInjection;

#[cfg(test)]
impl ForegroundRestoreFailureInjection {
    /// Sets restoration failure injection for the current test thread.
    pub(super) fn store(&self, enabled: bool, _ordering: Ordering) {
        FAIL_FOREGROUND_RESTORE_LOCAL.set(enabled);
    }

    /// Reads restoration failure injection for the current test thread.
    fn load(&self, _ordering: Ordering) -> bool {
        FAIL_FOREGROUND_RESTORE_LOCAL.get()
    }
}

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

/// Failure from a bounded subprocess, including terminal-ownership failures.
#[derive(Debug)]
pub(crate) enum BoundedCommandError {
    /// The command failed while Tau's foreground ownership was confirmed.
    Command(String),
    /// Tau could not confirm foreground ownership after settling the child.
    ForegroundOwnershipUnconfirmed {
        /// Child outcome or command error observed before restoration.
        primary: String,
        /// Error returned by the checked foreground-restoration attempt.
        restoration: ForegroundRestorationError,
    },
}

impl BoundedCommandError {
    /// Returns whether terminal input and redraw must remain paused.
    pub(crate) fn is_foreground_ownership_unconfirmed(&self) -> bool {
        matches!(self, Self::ForegroundOwnershipUnconfirmed { .. })
    }

    /// Returns the bounded restoration diagnostic for an ownership fail-stop.
    pub(crate) fn foreground_restoration_diagnostic(
        &self,
    ) -> Option<ForegroundRestorationDiagnostic> {
        match self {
            Self::ForegroundOwnershipUnconfirmed { restoration, .. } => {
                Some(restoration.diagnostic())
            }
            Self::Command(_) => None,
        }
    }
}

impl std::fmt::Display for BoundedCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Command(error) => formatter.write_str(error),
            Self::ForegroundOwnershipUnconfirmed {
                primary,
                restoration,
            } => write!(
                formatter,
                "terminal foreground ownership remains unconfirmed after {primary}: {restoration}"
            ),
        }
    }
}

impl std::error::Error for BoundedCommandError {}

impl From<String> for BoundedCommandError {
    fn from(error: String) -> Self {
        Self::Command(error)
    }
}

/// Bounded private diagnostic for one terminal foreground-restoration failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForegroundRestorationDiagnostic {
    /// Fixed failure class suitable for private operational logging.
    class: &'static str,
    /// Platform errno from the failed syscall, when one caused the failure.
    errno: Option<i32>,
}

impl ForegroundRestorationDiagnostic {
    /// Builds the fixed diagnostic for an unconfirmed `tcsetpgrp` failure.
    #[must_use]
    pub fn tcsetpgrp_unconfirmed(errno: i32) -> Self {
        Self {
            class: "tcsetpgrp-unconfirmed",
            errno: Some(errno),
        }
    }

    /// Returns the fixed restoration failure class.
    #[must_use]
    pub fn class(self) -> &'static str {
        self.class
    }

    /// Returns the platform errno when the failure originated from a syscall.
    #[must_use]
    pub fn errno(self) -> Option<i32> {
        self.errno
    }
}

/// Checked terminal foreground-restoration failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ForegroundRestorationError {
    /// Fixed private diagnostic class.
    class: &'static str,
    /// Platform error from the failed ownership syscall, when present.
    errno: Option<path_nix_errno::Errno>,
}

impl ForegroundRestorationError {
    /// Builds the bounded failure returned when `tcsetpgrp` remains
    /// unconfirmed.
    pub(crate) fn tcsetpgrp_unconfirmed(errno: path_nix_errno::Errno) -> Self {
        Self {
            class: "tcsetpgrp-unconfirmed",
            errno: Some(errno),
        }
    }

    /// Builds the failure returned when Tau was not initially in the
    /// foreground.
    fn initial_foreground_mismatch() -> Self {
        Self {
            class: "initial-foreground-mismatch",
            errno: None,
        }
    }

    /// Builds the failure returned when initial foreground ownership cannot be
    /// read.
    fn initial_foreground_unconfirmed(errno: path_nix_errno::Errno) -> Self {
        Self {
            class: "initial-foreground-unconfirmed",
            errno: Some(errno),
        }
    }

    /// Builds the failure returned when a failed handoff leaves ownership
    /// unknown.
    fn foreground_handoff_unconfirmed(errno: path_nix_errno::Errno) -> Self {
        Self {
            class: "foreground-handoff-unconfirmed",
            errno: Some(errno),
        }
    }

    /// Projects the bounded fields retained for private UI logging.
    fn diagnostic(self) -> ForegroundRestorationDiagnostic {
        ForegroundRestorationDiagnostic {
            class: self.class,
            errno: self.errno.map(|errno| errno as i32),
        }
    }
}

impl std::fmt::Display for ForegroundRestorationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.class)?;
        if let Some(errno) = self.errno {
            write!(formatter, ": {errno}")?;
        }
        Ok(())
    }
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
) -> Result<BoundedCommandOutput, BoundedCommandError> {
    run_with_bounded_stdout_after_spawn(
        command,
        stdin_input,
        stdout_limit,
        timeout,
        ownership,
        || Ok(()),
    )
}

/// Runs a bounded command after allowing a test fixture to observe its spawn.
pub(crate) fn run_with_bounded_stdout_after_spawn(
    command: &mut std::process::Command,
    stdin_input: Option<&[u8]>,
    stdout_limit: usize,
    timeout: std::time::Duration,
    ownership: ProcessOwnership,
    after_spawn: impl FnOnce() -> Result<(), String>,
) -> Result<BoundedCommandOutput, BoundedCommandError> {
    configure_process_ownership(command, ownership)?;
    if stdin_input.is_some() {
        command.stdin(path_std_process::Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|e| BoundedCommandError::Command(format!("could not spawn command: {e}")))?;
    let mut process_group = match claim_process_group_handle(ownership, child.id()) {
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
    if let Err(error) = after_spawn() {
        terminate_child(&mut child, process_group.child_pgid());
        return settle_after_child(&mut process_group, Err(error), |_| {
            unreachable!("after-spawn failure has no successful child output")
        });
    }

    match BoundedStdoutRun::start(child, process_group, stdin_input, stdout_limit) {
        Ok(run) => run.finish(timeout),
        Err(BoundedStdoutStartFailure {
            mut process_group,
            primary,
        }) => settle_after_child(&mut process_group, Err(primary), |_| {
            unreachable!("captured setup failure has no successful child output")
        }),
    }
}

/// State for one bounded stdout child run.
///
/// The direct-child waiter, stdout reader, and optional stdin writer each send
/// completion events into one channel. [`Self::finish`] blocks on that channel
/// with the command deadline as its `recv_timeout` limit, so child monitoring
/// does not need a poll/sleep loop. After the direct child exits, stdout and
/// stdin still receive short bounded post-exit grace waits; those waits detect
/// descendants that inherited prompt-owned pipes without letting Tau wait
/// indefinitely. Cleanup and post-exit waits may observe unrelated pending
/// events, so event handling must remain valid in every phase of the run.
struct BoundedStdoutRun {
    /// Owned process-group/foreground-terminal handle for the child.
    process_group: ProcessGroupHandle,
    /// Child/writer/reader event channel.
    event_rx: path_std_sync::mpsc::Receiver<BoundedStdoutEvent>,
    /// Completed stdout read observed before the direct child exited.
    stdout_result: Option<io::Result<LimitedRead>>,
    /// Whether the optional stdin writer has completed.
    stdin_done: bool,
    /// Completed direct-child wait result.
    child_result: Option<io::Result<std::process::ExitStatus>>,
    /// Maximum number of stdout bytes allowed before terminating the child.
    stdout_limit: usize,
}

/// Captured-command setup failure retaining foreground cleanup authority.
struct BoundedStdoutStartFailure {
    /// Process-group handle that must perform checked foreground restoration.
    process_group: ProcessGroupHandle,
    /// Primary setup failure observed after foreground transfer.
    primary: String,
}

impl BoundedStdoutRun {
    /// Starts background pipe handling for a spawned child.
    fn start(
        mut child: std::process::Child,
        process_group: ProcessGroupHandle,
        stdin_input: Option<&[u8]>,
        stdout_limit: usize,
    ) -> Result<Self, BoundedStdoutStartFailure> {
        let Some(stdout) = child.stdout.take() else {
            terminate_child(&mut child, process_group.child_pgid());
            return Err(BoundedStdoutStartFailure {
                process_group,
                primary: "command stdout was not captured".to_owned(),
            });
        };
        let (event_tx, event_rx) = path_std_sync::mpsc::channel();
        spawn_stdout_reader(stdout, stdout_limit, event_tx.clone());
        let stdin_done = match spawn_stdin_writer(&mut child, stdin_input, event_tx.clone()) {
            StdinWriterState::Done => true,
            StdinWriterState::Running => false,
        };
        spawn_bounded_stdout_child_waiter(child, event_tx);
        Ok(Self {
            process_group,
            event_rx,
            stdout_result: None,
            stdin_done,
            child_result: None,
            stdout_limit,
        })
    }

    /// Waits until the child exits, a pipe error occurs, or the timeout
    /// expires.
    fn finish(
        mut self,
        timeout: std::time::Duration,
    ) -> Result<BoundedCommandOutput, BoundedCommandError> {
        let result = self.wait_for_completion(timeout);
        settle_after_child(&mut self.process_group, result, |output| {
            format!("command exited with {}", output.status)
        })
    }

    fn wait_for_completion(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<BoundedCommandOutput, String> {
        let deadline = path_std_time::Instant::now() + timeout;
        loop {
            let Some(remaining) = deadline.checked_duration_since(path_std_time::Instant::now())
            else {
                self.terminate();
                self.wait_for_stdin_writer();
                return Err(format!("command exceeded {}s timeout", timeout.as_secs()));
            };

            match self.event_rx.recv_timeout(remaining) {
                Ok(event) => self.handle_event(event)?,
                Err(path_std_sync_mpsc::RecvTimeoutError::Timeout) => {
                    self.terminate();
                    self.wait_for_stdin_writer();
                    return Err(format!("command exceeded {}s timeout", timeout.as_secs()));
                }
                Err(path_std_sync_mpsc::RecvTimeoutError::Disconnected) => {
                    self.terminate();
                    self.wait_for_stdin_writer();
                    return Err("command monitor stopped unexpectedly".to_owned());
                }
            }

            if let Some(output) = self.take_child_output()? {
                return Ok(output);
            }
        }
    }

    /// Incorporates a completed child, stdin, or stdout event.
    fn handle_event(&mut self, event: BoundedStdoutEvent) -> Result<(), String> {
        match event {
            BoundedStdoutEvent::Stdout(Ok(stdout)) if stdout.overflowed => {
                self.terminate();
                self.wait_for_stdin_writer();
                Err(format!(
                    "command stdout exceeded {} bytes",
                    self.stdout_limit
                ))
            }
            BoundedStdoutEvent::Stdout(Err(error)) => {
                self.terminate();
                self.wait_for_stdin_writer();
                Err(format!("could not read command stdout: {error}"))
            }
            BoundedStdoutEvent::Stdout(result) => {
                self.stdout_result = Some(result);
                Ok(())
            }
            BoundedStdoutEvent::Stdin(Ok(())) => {
                self.stdin_done = true;
                Ok(())
            }
            BoundedStdoutEvent::Stdin(Err(error)) => {
                self.terminate();
                Err(format!("could not write to command stdin: {error}"))
            }
            BoundedStdoutEvent::Child(result) => {
                self.child_result = Some(result);
                Ok(())
            }
        }
    }

    /// Checks direct-child status and collects final pipe results after exit.
    fn take_child_output(&mut self) -> Result<Option<BoundedCommandOutput>, String> {
        let Some(result) = self.child_result.take() else {
            return Ok(None);
        };
        let status = match result {
            Ok(status) => status,
            Err(error) => {
                self.terminate_process_group_if_owned();
                return Err(format!("could not wait for command: {error}"));
            }
        };
        let stdout = match self.receive_stdout_after_child_exit() {
            Ok(stdout) => stdout,
            Err(error) => {
                self.terminate_process_group_if_owned();
                return Err(error);
            }
        };
        if let Err(error) = self.wait_for_stdin_after_child_exit() {
            self.terminate_process_group_if_owned();
            return Err(error);
        }
        Ok(Some(BoundedCommandOutput {
            status,
            stdout: stdout.bytes,
        }))
    }

    /// Waits briefly for stdout EOF after the direct child has exited.
    fn receive_stdout_after_child_exit(&mut self) -> Result<LimitedRead, String> {
        while self.stdout_result.is_none() {
            let event = self
                .event_rx
                .recv_timeout(POST_EXIT_PIPE_CLOSE_TIMEOUT)
                .map_err(|_| "command stdout pipe did not close after child exit".to_owned())?;
            self.handle_event(event)?;
        }
        let stdout_result = self
            .stdout_result
            .take()
            .expect("stdout result must be available after post-exit wait");
        receive_stdout_result(stdout_result, self.stdout_limit)
    }

    /// Waits briefly for the stdin writer after the direct child has exited.
    fn wait_for_stdin_after_child_exit(&mut self) -> Result<(), String> {
        while !self.stdin_done {
            let event = self
                .event_rx
                .recv_timeout(POST_EXIT_PIPE_CLOSE_TIMEOUT)
                .map_err(|_| "command stdin pipe did not close after child exit".to_owned())?;
            self.handle_event(event)?;
        }
        Ok(())
    }

    /// Terminates the direct child or its process group and reaps it.
    fn terminate(&mut self) {
        terminate_process_group_if_owned(self.process_group.child_pgid());
        let _ = self.wait_for_child_after_terminate();
    }

    /// Terminates the owned process group without waiting for the
    /// already-exited child.
    fn terminate_process_group_if_owned(&self) {
        terminate_process_group_if_owned(self.process_group.child_pgid());
    }

    /// Waits briefly for an in-flight stdin writer after child termination.
    fn wait_for_stdin_writer(&mut self) {
        let _ = self.wait_for_stdin_after_child_exit();
    }

    /// Waits briefly for the child waiter after sending termination.
    fn wait_for_child_after_terminate(&mut self) -> Result<(), String> {
        while self.child_result.is_none() {
            let event = self
                .event_rx
                .recv_timeout(POST_EXIT_PIPE_CLOSE_TIMEOUT)
                .map_err(|_| "command child did not exit after termination".to_owned())?;
            self.handle_event(event)?;
        }
        Ok(())
    }
}

/// Events emitted by bounded stdout helper threads.
enum BoundedStdoutEvent {
    /// Direct child completed.
    Child(io::Result<std::process::ExitStatus>),
    /// Captured stdout reader completed.
    Stdout(io::Result<LimitedRead>),
    /// Optional stdin writer completed.
    Stdin(io::Result<()>),
}

/// State of the optional stdin writer after setup.
enum StdinWriterState {
    /// No stdin writer is active.
    Done,
    /// A stdin writer thread was spawned and will report completion.
    Running,
}

fn spawn_stdout_reader(
    stdout: impl Read + Send + 'static,
    stdout_limit: usize,
    event_tx: path_std_sync::mpsc::Sender<BoundedStdoutEvent>,
) {
    std::thread::spawn(move || {
        let _ = event_tx.send(BoundedStdoutEvent::Stdout(read_to_limit(
            stdout,
            stdout_limit,
        )));
    });
}

fn spawn_stdin_writer(
    child: &mut std::process::Child,
    input: Option<&[u8]>,
    event_tx: path_std_sync::mpsc::Sender<BoundedStdoutEvent>,
) -> StdinWriterState {
    let Some(input) = input else {
        return StdinWriterState::Done;
    };
    let Some(mut stdin) = child.stdin.take() else {
        return StdinWriterState::Done;
    };
    let input = input.to_vec();
    std::thread::spawn(move || {
        let result = match stdin.write_all(&input) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            Err(error) => Err(error),
        };
        let _ = event_tx.send(BoundedStdoutEvent::Stdin(result));
    });
    StdinWriterState::Running
}

fn spawn_bounded_stdout_child_waiter(
    mut child: std::process::Child,
    event_tx: path_std_sync::mpsc::Sender<BoundedStdoutEvent>,
) {
    std::thread::spawn(move || {
        let _ = event_tx.send(BoundedStdoutEvent::Child(child.wait()));
    });
}

fn spawn_child_waiter(
    mut child: std::process::Child,
) -> path_std_sync::mpsc::Receiver<io::Result<std::process::ExitStatus>> {
    let (tx, rx) = path_std_sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    rx
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
) -> Result<BoundedCommandStatus, BoundedCommandError> {
    run_with_inherited_stdio_after_spawn(command, timeout, ownership, || Ok(()))
}

/// Runs an inherited-stdio command after exposing deterministic child
/// readiness.
fn run_with_inherited_stdio_after_spawn(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    ownership: ProcessOwnership,
    after_spawn: impl FnOnce() -> Result<(), String>,
) -> Result<BoundedCommandStatus, BoundedCommandError> {
    configure_process_ownership(command, ownership)?;
    let mut child = command
        .spawn()
        .map_err(|e| BoundedCommandError::Command(format!("could not spawn command: {e}")))?;
    let mut process_group = match claim_process_group_handle(ownership, child.id()) {
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
    if let Err(error) = after_spawn() {
        terminate_child(&mut child, process_group.child_pgid());
        return settle_after_child(&mut process_group, Err(error), |_| {
            unreachable!("after-spawn failure has no successful child status")
        });
    }

    let child_rx = spawn_child_waiter(child);
    let result = match child_rx.recv_timeout(timeout) {
        Ok(Ok(status)) => Ok(BoundedCommandStatus { status }),
        Ok(Err(error)) => {
            terminate_process_group_if_owned(process_group.child_pgid());
            Err(format!("could not wait for command: {error}"))
        }
        Err(path_std_sync_mpsc::RecvTimeoutError::Timeout) => {
            terminate_process_group_if_owned(process_group.child_pgid());
            let _ = child_rx.recv_timeout(POST_EXIT_PIPE_CLOSE_TIMEOUT);
            Err(format!("command exceeded {}s timeout", timeout.as_secs()))
        }
        Err(path_std_sync_mpsc::RecvTimeoutError::Disconnected) => {
            terminate_process_group_if_owned(process_group.child_pgid());
            Err("command monitor stopped unexpectedly".to_owned())
        }
    };
    settle_after_child(&mut process_group, result, |output| {
        format!("command exited with {}", output.status)
    })
}

/// Restores foreground ownership after a child has been fully settled.
fn settle_after_child<T>(
    process_group: &mut ProcessGroupHandle,
    primary: Result<T, String>,
    describe_success: impl FnOnce(&T) -> String,
) -> Result<T, BoundedCommandError> {
    settle_after_child_with_restore(primary, describe_success, || {
        process_group.restore_foreground()
    })
}

/// Combines the primary child result with one checked restoration attempt.
fn settle_after_child_with_restore<T>(
    primary: Result<T, String>,
    describe_success: impl FnOnce(&T) -> String,
    restore_foreground: impl FnOnce() -> Result<(), ForegroundRestorationError>,
) -> Result<T, BoundedCommandError> {
    let primary_description = match &primary {
        Ok(value) => describe_success(value),
        Err(error) => format!("command failure ({error})"),
    };
    match restore_foreground() {
        Ok(()) => primary.map_err(BoundedCommandError::Command),
        Err(restoration) => Err(BoundedCommandError::ForegroundOwnershipUnconfirmed {
            primary: primary_description,
            restoration,
        }),
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

fn receive_stdout_result(
    stdout_result: io::Result<LimitedRead>,
    stdout_limit: usize,
) -> Result<LimitedRead, String> {
    let stdout = stdout_result.map_err(|e| format!("could not read command stdout: {e}"))?;
    if stdout.overflowed {
        return Err(format!("command stdout exceeded {} bytes", stdout_limit));
    }
    Ok(stdout)
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
        path_nix_unistd::Pid::from_raw(self.0)
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
        let _ = path_nix_sys::signal::killpg(pgid, path_nix_sys_signal::Signal::SIGTERM);
        std::thread::sleep(path_std_time::Duration::from_millis(100));
        let _ = path_nix_sys::signal::killpg(pgid, path_nix_sys_signal::Signal::SIGKILL);
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
    fn new(ownership: ProcessOwnership, child_id: u32) -> Result<Self, BoundedCommandError> {
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

    fn restore_foreground(&mut self) -> Result<(), ForegroundRestorationError> {
        #[cfg(unix)]
        if let Some(parent_pgid) = self.parent_pgid {
            #[cfg(test)]
            if FAIL_FOREGROUND_RESTORE.load(Ordering::SeqCst) {
                FOREGROUND_RESTORE_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
                return Err(ForegroundRestorationError::tcsetpgrp_unconfirmed(
                    path_nix_errno::Errno::EIO,
                ));
            }
            restore_foreground_process_group(parent_pgid)?;
            self.parent_pgid = None;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn claim_foreground(child_id: u32) -> Result<Self, BoundedCommandError> {
        let tau_pgid = nix::unistd::getpgrp();
        let child_pgid = ChildProcessGroupId::from_child_id(child_id);
        let claimed_foreground = claim_foreground_process_group(tau_pgid, child_pgid.as_nix_pid())?;
        if claimed_foreground {
            let _ = path_nix_sys::signal::killpg(
                child_pgid.as_nix_pid(),
                path_nix_sys_signal::Signal::SIGCONT,
            );
        }
        #[cfg(test)]
        let claimed_foreground =
            claimed_foreground || FAIL_FOREGROUND_RESTORE.load(Ordering::SeqCst);
        Ok(Self {
            child_pgid: Some(child_pgid),
            parent_pgid: claimed_foreground.then_some(tau_pgid),
        })
    }

    #[cfg(not(unix))]
    fn claim_foreground(_child_id: u32) -> Result<Self, BoundedCommandError> {
        Err(BoundedCommandError::Command(
            "foreground process-group prompt actions are unsupported on this platform".to_owned(),
        ))
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupHandle {
    fn drop(&mut self) {
        let _ = self.restore_foreground();
    }
}

#[cfg(unix)]
fn claim_foreground_process_group(
    tau_pgid: nix::unistd::Pid,
    child_pgid: nix::unistd::Pid,
) -> Result<bool, BoundedCommandError> {
    match path_std_fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(tty) => claim_foreground_process_group_with(
            tau_pgid,
            || nix::unistd::tcgetpgrp(tty.as_fd()),
            || tcsetpgrp_blocking_sigtou(tty.as_fd(), child_pgid),
        ),
        Err(_) => claim_foreground_process_group_with(
            tau_pgid,
            || nix::unistd::tcgetpgrp(std::io::stdin().as_fd()),
            || tcsetpgrp_blocking_sigtou(std::io::stdin().as_fd(), child_pgid),
        ),
    }
}

#[cfg(unix)]
fn claim_foreground_process_group_with(
    tau_pgid: nix::unistd::Pid,
    mut get_foreground: impl FnMut() -> nix::Result<nix::unistd::Pid>,
    mut set_child_foreground: impl FnMut() -> nix::Result<()>,
) -> Result<bool, BoundedCommandError> {
    loop {
        match get_foreground() {
            Ok(foreground) if foreground == tau_pgid => break,
            Ok(_) => {
                return Err(BoundedCommandError::ForegroundOwnershipUnconfirmed {
                    primary: "prompt action foreground handoff".to_owned(),
                    restoration: ForegroundRestorationError::initial_foreground_mismatch(),
                });
            }
            Err(path_nix_errno::Errno::EINTR) => {}
            Err(path_nix_errno::Errno::ENOTTY) => return Ok(false),
            Err(error) => {
                return Err(BoundedCommandError::ForegroundOwnershipUnconfirmed {
                    primary: "prompt action foreground handoff".to_owned(),
                    restoration: ForegroundRestorationError::initial_foreground_unconfirmed(error),
                });
            }
        }
    }

    let set_error = loop {
        match set_child_foreground() {
            Ok(()) => return Ok(true),
            Err(path_nix_errno::Errno::EINTR) => {}
            Err(error) => break error,
        }
    };
    let tau_still_foreground = loop {
        match get_foreground() {
            Ok(foreground) => break foreground == tau_pgid,
            Err(path_nix_errno::Errno::EINTR) => {}
            Err(_) => break false,
        }
    };
    if tau_still_foreground {
        Err(BoundedCommandError::Command(format!(
            "could not hand terminal to prompt action: {set_error}"
        )))
    } else {
        Err(BoundedCommandError::ForegroundOwnershipUnconfirmed {
            primary: "prompt action foreground handoff".to_owned(),
            restoration: ForegroundRestorationError::foreground_handoff_unconfirmed(set_error),
        })
    }
}

#[cfg(unix)]
fn restore_foreground_process_group(
    pgid: nix::unistd::Pid,
) -> Result<(), ForegroundRestorationError> {
    match path_std_fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(tty) => restore_foreground_process_group_with(
            pgid,
            || tcsetpgrp_blocking_sigtou(tty.as_fd(), pgid),
            || nix::unistd::tcgetpgrp(tty.as_fd()),
        ),
        Err(_) => restore_foreground_process_group_with(
            pgid,
            || tcsetpgrp_blocking_sigtou(std::io::stdin().as_fd(), pgid),
            || nix::unistd::tcgetpgrp(std::io::stdin().as_fd()),
        ),
    }
}

#[cfg(unix)]
fn restore_foreground_process_group_with(
    pgid: nix::unistd::Pid,
    mut set_foreground: impl FnMut() -> nix::Result<()>,
    mut get_foreground: impl FnMut() -> nix::Result<nix::unistd::Pid>,
) -> Result<(), ForegroundRestorationError> {
    let set_error = loop {
        match set_foreground() {
            Ok(()) => return Ok(()),
            Err(path_nix_errno::Errno::EINTR) => {}
            Err(error) => break error,
        }
    };
    let foreground = loop {
        match get_foreground() {
            Ok(foreground) => break Some(foreground),
            Err(path_nix_errno::Errno::EINTR) => {}
            Err(_) => break None,
        }
    };
    if foreground == Some(pgid) {
        Ok(())
    } else {
        Err(ForegroundRestorationError::tcsetpgrp_unconfirmed(set_error))
    }
}

#[cfg(unix)]
fn tcsetpgrp_blocking_sigtou(
    fd: path_std_os::fd::BorrowedFd<'_>,
    pgid: nix::unistd::Pid,
) -> nix::Result<()> {
    let mut block = path_nix_sys_signal::SigSet::empty();
    block.add(path_nix_sys_signal::Signal::SIGTTOU);
    let mut previous = path_nix_sys_signal::SigSet::empty();
    path_nix_sys::signal::pthread_sigmask(
        path_nix_sys_signal::SigmaskHow::SIG_BLOCK,
        Some(&block),
        Some(&mut previous),
    )?;
    let result = nix::unistd::tcsetpgrp(fd, pgid);
    let restore = path_nix_sys::signal::pthread_sigmask(
        path_nix_sys_signal::SigmaskHow::SIG_SETMASK,
        Some(&previous),
        None,
    );
    result.and(restore)
}

fn claim_process_group_handle(
    ownership: ProcessOwnership,
    child_id: u32,
) -> Result<ProcessGroupHandle, BoundedCommandError> {
    #[cfg(test)]
    if matches!(ownership, ProcessOwnership::ForegroundProcessGroup)
        && FAIL_NEXT_FOREGROUND_CLAIM.swap(false, Ordering::SeqCst)
    {
        return Err(BoundedCommandError::Command(
            "could not hand terminal to prompt action: injected failure".to_owned(),
        ));
    }
    ProcessGroupHandle::new(ownership, child_id)
}

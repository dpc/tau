//! Supervised child-process management and stdio transport adapters.
//!
//! The initial implementation focuses on one supervised child process connected
//! over stdin/stdout using the shared CBOR event protocol.
//!
//! This crate is not currently wired into the production harness extension
//! supervisor path. Treat its tests as prototype coverage until the harness
//! integrates this crate or duplicates the same contracts.
//! Lifecycle, transport, and trust boundaries are summarized in
//! `ARCH-tau-supervisor`.

use std::io::{self, BufReader, BufWriter};
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;
use std::{fmt, thread};

#[cfg(unix)]
#[cfg(target_os = "linux")]
use rustix::process::{self, Pid, PidfdFlags, Signal};
use tau_proto::{
    DecodeError, Event, ExtensionExited, ExtensionName, ExtensionReady, ExtensionStarting,
    HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
};

const STDOUT_FRAME_BUFFER: usize = 64;

/// Child stderr handling policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StderrPolicy {
    /// Inherit the supervisor process stderr handle.
    Inherit,
    /// Discard child stderr output.
    Null,
}

/// One configured supervised extension command.
///
/// Spawned children inherit the supervisor process environment except variables
/// whose names start with `TAU_SECRET_`; those are removed before launch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionCommand {
    /// Stable extension identity used in lifecycle events.
    pub name: ExtensionName,
    /// Executable path passed to `Command::new` when spawning the child.
    pub program: PathBuf,
    /// Command-line arguments passed to the child after `program`.
    pub args: Vec<String>,
    /// Optional working directory for the child process.
    pub working_dir: Option<PathBuf>,
    /// Policy for child stderr output.
    pub stderr: StderrPolicy,
}

impl ExtensionCommand {
    /// Creates a pre-spawn lifecycle event when no child pid is available yet.
    #[must_use]
    pub fn pre_spawn_starting_event(&self, instance_id: tau_proto::ExtensionInstanceId) -> Event {
        Event::ExtensionStarting(ExtensionStarting {
            instance_id,
            extension_name: self.name.clone(),
            pid: None,
        })
    }
}

/// One detected child-process exit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildExit {
    /// Numeric process exit code when the platform reports normal exit.
    exit_code: Option<i32>,
    /// Unix signal number when the process was terminated by signal.
    signal: Option<i32>,
}

impl ChildExit {
    fn from_status(status: std::process::ExitStatus) -> Self {
        Self {
            exit_code: status.code(),
            signal: exit_signal(status),
        }
    }

    /// Returns the numeric process exit code when the platform reports normal
    /// exit.
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Returns the Unix signal number when the process was terminated by a
    /// signal.
    ///
    /// Non-Unix platforms always return `None`.
    #[must_use]
    pub fn signal(&self) -> Option<i32> {
        self.signal
    }
}

/// Outcome of a timed receive attempt from a supervised child's stdout.
#[derive(Clone, Debug, PartialEq)]
pub enum ReceiveOutcome {
    /// A complete extension-to-harness protocol message was decoded.
    Message(Box<HarnessInputMessage>),
    /// No stdout message arrived before the requested timeout elapsed.
    Timeout,
    /// The child closed stdout cleanly at a protocol message boundary.
    Closed,
}

/// Errors produced by the supervised stdio transport.
#[derive(Debug)]
pub enum SupervisionError {
    /// The command used a relative program path with an explicit working
    /// directory.
    RelativeProgramWithWorkingDir {
        /// Relative program path that would have platform-specific resolution.
        program: PathBuf,
        /// Requested child working directory.
        working_dir: PathBuf,
    },
    /// The child process could not be spawned.
    Spawn(io::Error),
    /// The spawned child did not provide a piped stdin handle.
    MissingStdin,
    /// The spawned child did not provide a piped stdout handle.
    MissingStdout,
    /// A harness-to-extension protocol message could not be encoded.
    Encode(tau_proto::EncodeError),
    /// The child stdin writer could not be flushed after sending a message.
    Flush(io::Error),
    /// The child stdout reader thread could not be started.
    ReaderThread(io::Error),
    /// The child process waiter thread could not be started.
    WaiterThread(io::Error),
    /// The child stdout reader observed corrupt or truncated protocol data.
    Decode(DecodeError),
    /// The child process could not be killed during hard termination.
    Kill(io::Error),
    /// Waiting for child process status failed.
    Wait(io::Error),
    /// The child did not exit before the requested timeout elapsed.
    Timeout {
        /// Timeout duration requested by the caller.
        duration: Duration,
    },
}

impl fmt::Display for SupervisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RelativeProgramWithWorkingDir {
                program,
                working_dir,
            } => write!(
                f,
                "relative child program path `{}` cannot be combined with working directory `{}`",
                program.display(),
                working_dir.display()
            ),
            Self::Spawn(source) => write!(f, "failed to spawn child process: {source}"),
            Self::MissingStdin => f.write_str("spawned child process did not expose stdin"),
            Self::MissingStdout => f.write_str("spawned child process did not expose stdout"),
            Self::Encode(source) => write!(f, "failed to encode event for child stdin: {source}"),
            Self::Flush(source) => write!(f, "failed to flush child stdin: {source}"),
            Self::ReaderThread(source) => {
                write!(f, "failed to start child stdout reader thread: {source}")
            }
            Self::WaiterThread(source) => {
                write!(f, "failed to start child waiter thread: {source}")
            }
            Self::Decode(source) => write!(f, "failed to decode event from child stdout: {source}"),
            Self::Kill(source) => write!(f, "failed to kill child process: {source}"),
            Self::Wait(source) => write!(f, "failed to wait for child process: {source}"),
            Self::Timeout { duration } => {
                write!(f, "timed out waiting for child exit after {duration:?}")
            }
        }
    }
}

impl std::error::Error for SupervisionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RelativeProgramWithWorkingDir { .. } => None,
            Self::Spawn(source) => Some(source),
            Self::MissingStdin => None,
            Self::MissingStdout => None,
            Self::Encode(source) => Some(source),
            Self::Flush(source) => Some(source),
            Self::ReaderThread(source) => Some(source),
            Self::WaiterThread(source) => Some(source),
            Self::Decode(source) => Some(source),
            Self::Kill(source) => Some(source),
            Self::Wait(source) => Some(source),
            Self::Timeout { .. } => None,
        }
    }
}

/// One supervised child process connected over stdin/stdout.
///
/// The child lifecycle is owned by this value, while a dedicated waiter thread
/// owns the `Child` handle and reports exit status through a channel. On Linux,
/// a pidfd is opened before spawn cleanup is disarmed, so hard termination can
/// signal the same direct child without a PID-reuse race. Callers should prefer
/// explicit graceful protocol shutdown or `terminate`; `Drop` only hard-kills
/// live children on Linux, waits best-effort after a successful hard-kill
/// signal, and ignores cleanup errors. Non-Linux hard-kill cleanup is
/// unsupported in this waiter-thread implementation. Cleanup targets only the
/// owned direct child process, not a process group, process tree, or
/// grandchildren.
/// Child stdout is handed through a bounded buffer, so callers supervising a
/// child that can emit during shutdown must continue calling `recv_timeout` or
/// otherwise drain stdout before waiting indefinitely for exit.
pub struct SupervisedChild {
    command: ExtensionCommand,
    pid: u32,
    #[cfg(target_os = "linux")]
    pidfd: OwnedFd,
    stdin: HarnessOutputWriter<BufWriter<ChildStdin>>,
    stdout_frames: Receiver<Result<StdoutFrame, DecodeError>>,
    exit: Receiver<Result<ChildExit, io::Error>>,
    observed_exit: Option<ChildExit>,
}
impl SupervisedChild {
    /// Spawns one supervised child process with piped stdin/stdout.
    ///
    /// # Errors
    ///
    /// Returns an error if the process cannot be spawned, required pipe handles
    /// are missing, the stdout reader or child waiter thread cannot be started,
    /// Linux pidfd setup fails, or a relative program path is combined with an
    /// explicit working directory. Spawn initialization failures after process
    /// creation kill/wait the partially initialized child before returning.
    pub fn spawn(command: ExtensionCommand) -> Result<Self, SupervisionError> {
        let mut child_command = Command::new(&command.program);
        child_command
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(match command.stderr {
                StderrPolicy::Inherit => Stdio::inherit(),
                StderrPolicy::Null => Stdio::null(),
            });
        if let Some(working_dir) = &command.working_dir {
            if command.program.is_relative() {
                return Err(SupervisionError::RelativeProgramWithWorkingDir {
                    program: command.program.clone(),
                    working_dir: working_dir.clone(),
                });
            }
            child_command.current_dir(working_dir);
        }
        remove_secret_env(&mut child_command);

        let child = child_command.spawn().map_err(SupervisionError::Spawn)?;
        let mut child_guard = SpawnedChildGuard::new(child);

        let stdin = child_guard
            .child_mut()
            .stdin
            .take()
            .ok_or(SupervisionError::MissingStdin)?;
        let stdout = child_guard
            .child_mut()
            .stdout
            .take()
            .ok_or(SupervisionError::MissingStdout)?;
        let stdout_frames = spawn_stdout_reader(stdout)?;
        let pid = child_guard.child_mut().id();
        #[cfg(target_os = "linux")]
        let pidfd = open_pidfd(pid)?;
        let exit = spawn_child_waiter(child_guard)?;

        Ok(Self {
            command,
            pid,
            #[cfg(target_os = "linux")]
            pidfd,
            stdin: HarnessOutputWriter::new(BufWriter::new(stdin)),
            stdout_frames,
            exit,
            observed_exit: None,
        })
    }

    /// Returns the extension command used to launch this child.
    #[must_use]
    pub fn command(&self) -> &ExtensionCommand {
        &self.command
    }

    /// Returns the child process ID.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Creates the lifecycle event emitted after the child process has started.
    #[must_use]
    pub fn starting_event(&self, instance_id: tau_proto::ExtensionInstanceId) -> Event {
        Event::ExtensionStarting(ExtensionStarting {
            instance_id,
            extension_name: self.command.name.clone(),
            pid: Some(self.pid()),
        })
    }

    /// Creates the lifecycle event emitted when the child becomes connected.
    #[must_use]
    pub fn ready_event(&self, instance_id: tau_proto::ExtensionInstanceId) -> Event {
        Event::ExtensionReady(ExtensionReady {
            instance_id,
            extension_name: self.command.name.clone(),
            pid: Some(self.pid()),
        })
    }

    /// Sends one harness → extension protocol message to the child over stdin.
    ///
    /// # Errors
    ///
    /// Returns an error if the message cannot be encoded or child stdin cannot
    /// be flushed.
    pub fn send(&mut self, message: &HarnessOutputMessage) -> Result<(), SupervisionError> {
        self.stdin
            .write_message(message)
            .map_err(SupervisionError::Encode)?;
        self.stdin.flush().map_err(SupervisionError::Flush)
    }

    /// Reads one extension → harness protocol message from the child.
    ///
    /// Timeouts, clean stdout closure, and decoded messages are returned as
    /// distinct outcomes. Truncated or corrupt frames are reported as decode
    /// errors.
    ///
    /// # Errors
    ///
    /// Returns an error if the stdout reader observes corrupt or truncated
    /// protocol data.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<ReceiveOutcome, SupervisionError> {
        match self.stdout_frames.recv_timeout(timeout) {
            Ok(Ok(StdoutFrame::Message(frame))) => Ok(ReceiveOutcome::Message(frame)),
            Ok(Ok(StdoutFrame::Closed)) => Ok(ReceiveOutcome::Closed),
            Ok(Err(error)) => Err(SupervisionError::Decode(error)),
            Err(RecvTimeoutError::Timeout) => Ok(ReceiveOutcome::Timeout),
            Err(RecvTimeoutError::Disconnected) => Ok(ReceiveOutcome::Closed),
        }
    }

    /// Checks whether the child has already exited.
    ///
    /// This observes the spawn-time waiter thread notification. Once an exit is
    /// observed, later `try_wait` and `wait_for_exit` calls return the cached
    /// status instead of consuming the one-shot notification again.
    ///
    /// # Errors
    ///
    /// Returns an error if the waiter thread reports an operating-system wait
    /// failure or exits without sending a child status notification.
    pub fn try_wait(&mut self) -> Result<Option<ChildExit>, SupervisionError> {
        if let Some(exit) = &self.observed_exit {
            return Ok(Some(exit.clone()));
        }
        match self.exit.try_recv() {
            Ok(Ok(exit)) => {
                self.observed_exit = Some(exit.clone());
                Ok(Some(exit))
            }
            Ok(Err(error)) => Err(SupervisionError::Wait(error)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err(SupervisionError::Wait(child_waiter_disconnected()))
            }
        }
    }

    /// Waits until the child exits or the timeout elapses.
    ///
    /// This blocks on the spawn-time waiter thread notification. Once an exit
    /// is observed, later `try_wait` and `wait_for_exit` calls return the
    /// cached status instead of consuming the one-shot notification again.
    ///
    /// # Errors
    ///
    /// Returns an error if the waiter thread reports an operating-system wait
    /// failure, exits without sending a child status notification, or the
    /// timeout elapses before the child exits.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Result<ChildExit, SupervisionError> {
        if let Some(exit) = &self.observed_exit {
            return Ok(exit.clone());
        }
        match self.exit.recv_timeout(timeout) {
            Ok(Ok(exit)) => {
                self.observed_exit = Some(exit.clone());
                Ok(exit)
            }
            Ok(Err(error)) => Err(SupervisionError::Wait(error)),
            Err(RecvTimeoutError::Timeout) => Err(SupervisionError::Timeout { duration: timeout }),
            Err(RecvTimeoutError::Disconnected) => {
                Err(SupervisionError::Wait(child_waiter_disconnected()))
            }
        }
    }

    /// Creates the lifecycle event emitted when the child exits.
    #[must_use]
    pub fn exited_event(
        &self,
        instance_id: tau_proto::ExtensionInstanceId,
        exit: &ChildExit,
    ) -> Event {
        Event::ExtensionExited(ExtensionExited {
            instance_id,
            extension_name: self.command.name.clone(),
            pid: Some(self.pid()),
            exit_code: exit.exit_code,
            signal: exit.signal,
        })
    }

    /// Forcibly terminates the child process and waits for its exit.
    ///
    /// This is the explicit hard-shutdown API for callers that decide graceful
    /// protocol shutdown is no longer possible or no longer desired. It kills
    /// only the owned direct child process, not a process group, process tree,
    /// or grandchildren.
    /// On Linux, termination uses the pidfd opened during spawn to avoid
    /// signaling a reused numeric PID after the waiter has reaped the child.
    /// Hard termination is unsupported on non-Linux targets in the current
    /// waiter-thread implementation.
    ///
    /// # Errors
    ///
    /// Returns an error if checking child status fails, killing the process
    /// fails, hard termination is requested on a non-Linux target, or the child
    /// does not exit before the timeout.
    pub fn terminate(&mut self, timeout: Duration) -> Result<ChildExit, SupervisionError> {
        if let Some(exit) = self.try_wait()? {
            return Ok(exit);
        }
        self.kill_child().map_err(SupervisionError::Kill)?;
        self.wait_for_exit(timeout)
    }

    #[cfg(target_os = "linux")]
    fn kill_child(&self) -> io::Result<()> {
        kill_pidfd(&self.pidfd)
    }

    #[cfg(not(target_os = "linux"))]
    fn kill_child(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reactive child termination is not implemented on this platform",
        ))
    }
}

impl Drop for SupervisedChild {
    // Performs last-resort cleanup for children that callers did not shut
    // down.
    fn drop(&mut self) {
        match self.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if self.kill_child().is_ok() {
                    // This call is intentionally best-effort; preserve the existing discarded
                    // result. ast-grep-ignore: let-underscore-call
                    let _ = self.wait_for_exit(Duration::MAX);
                }
            }
            Err(_) => {}
        }
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> Result<OwnedFd, SupervisionError> {
    let pid = pid_to_rustix_pid(pid).map_err(SupervisionError::Spawn)?;
    process::pidfd_open(pid, PidfdFlags::empty()).map_err(|error| {
        SupervisionError::Spawn(io::Error::from_raw_os_error(error.raw_os_error()))
    })
}

fn spawn_child_waiter(
    mut child_guard: SpawnedChildGuard,
) -> Result<Receiver<Result<ChildExit, io::Error>>, SupervisionError> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("tau-supervisor-child-wait".to_owned())
        .spawn(move || {
            let result = child_guard.wait_for_exit();
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = sender.send(result);
        })
        .map_err(SupervisionError::WaiterThread)?;
    Ok(receiver)
}

fn child_waiter_disconnected() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "child waiter thread exited without reporting status",
    )
}

#[cfg(target_os = "linux")]
fn kill_pidfd(pidfd: &OwnedFd) -> io::Result<()> {
    match process::pidfd_send_signal(pidfd, Signal::Kill) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(io::Error::from_raw_os_error(error.raw_os_error())),
    }
}

#[cfg(target_os = "linux")]
fn pid_to_rustix_pid(pid: u32) -> io::Result<Pid> {
    // Preserve this behavior; the structural alternative is not semantics-neutral
    // here. ast-grep-ignore: silent-map-err
    let pid = i32::try_from(pid).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("child pid {pid} does not fit platform pid type"),
        )
    })?;
    Pid::from_raw(pid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pid was not positive"))
}

// Owns a child during spawn and waiter startup. It kills/waits unless the
// waiter thread has already successfully waited for the child.
struct SpawnedChildGuard {
    child: Option<Child>,
    waited: bool,
}

impl SpawnedChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            waited: false,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("guard always holds child")
    }

    fn wait_for_exit(&mut self) -> Result<ChildExit, io::Error> {
        let result = self.child_mut().wait().map(ChildExit::from_status);
        if result.is_ok() {
            self.mark_waited();
        }
        result
    }

    fn mark_waited(&mut self) {
        self.waited = true;
    }
}

impl Drop for SpawnedChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child
            && !self.waited
        {
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = child.kill();
            // This call is intentionally best-effort; preserve the existing discarded
            // result. ast-grep-ignore: let-underscore-call
            let _ = child.wait();
        }
    }
}

fn remove_secret_env(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TAU_SECRET_") {
            command.env_remove(key);
        }
    }
}

enum StdoutFrame {
    Message(Box<HarnessInputMessage>),
    Closed,
}

fn spawn_stdout_reader(
    stdout: std::process::ChildStdout,
) -> Result<Receiver<Result<StdoutFrame, DecodeError>>, SupervisionError> {
    let (sender, receiver) = mpsc::sync_channel(STDOUT_FRAME_BUFFER);
    thread::Builder::new()
        .name("tau-supervisor-stdout".to_owned())
        .spawn(move || {
            let mut reader = HarnessInputReader::new(BufReader::new(stdout));
            loop {
                match reader.read_message() {
                    Ok(Some(frame)) => {
                        if sender
                            .send(Ok(StdoutFrame::Message(Box::new(frame))))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(None) => {
                        // This call is intentionally best-effort; preserve the existing discarded
                        // result. ast-grep-ignore: let-underscore-call
                        let _ = sender.send(Ok(StdoutFrame::Closed));
                        return;
                    }
                    Err(error) => {
                        // This call is intentionally best-effort; preserve the existing discarded
                        // result. ast-grep-ignore: let-underscore-call
                        let _ = sender.send(Err(error));
                        return;
                    }
                }
            }
        })
        .map_err(SupervisionError::ReaderThread)?;
    Ok(receiver)
}

#[cfg(unix)]
fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

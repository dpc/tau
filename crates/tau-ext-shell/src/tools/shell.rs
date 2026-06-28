//! `shell` tool and user-initiated `!`/`!!` command dispatch.

use std::sync::mpsc;

use tau_proto::{
    CborValue, Event, HarnessInputMessage, ToolUsePayload, ToolUseState, ToolUseStatus,
};
use tracing::{debug, trace};

use crate::argument::{argument_text, optional_argument_int_strict, optional_argument_text};
use crate::config::ShellConfig;
use crate::display::{ToolFailure, ToolOutput, ok_display, text_stats};
use crate::tools::world::{ShellWorld, WorldShellOutcome};
use crate::truncate::{MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, truncate_line_oriented_lines};

pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub(crate) const SLOW_COMMAND_EXEC_TIME_THRESHOLD_SECS: u64 = 5;
const VCR_REPLAY_SPEEDUP: u64 = 100;
const MAX_CAPTURED_LINE_BYTES: usize = MAX_OUTPUT_BYTES;
const USER_OUTPUT_TRUNCATED_MARKER: &str = "[output truncated]";
/// Filesystem access mode ext-shell infers for a shell command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellAccessMode {
    /// Command is treated as read-only filesystem access.
    ReadOnly,
    /// Command is treated as read-write filesystem access.
    ReadWrite,
}

impl ShellAccessMode {
    fn display_label(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }
}

/// Shell mode plus whether the UI should render the mode chip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellCommandMode {
    /// Read-write execution with no UI mode chip. Used when `dir_lock` is
    /// disabled and shell commands behave like ordinary host commands.
    HiddenReadWrite,
    /// Inferred execution mode shown in UI state. Used when `dir_lock` is
    /// enabled, so the mode communicates lock-derived shell semantics.
    Visible(ShellAccessMode),
}

impl ShellCommandMode {
    /// Read-write shell execution with no UI mode chip.
    pub(crate) const READ_WRITE_HIDDEN: Self = Self::HiddenReadWrite;

    /// Build a visible inferred mode for directory-lock-enabled shells.
    pub(crate) fn visible(access: ShellAccessMode) -> Self {
        Self::Visible(access)
    }

    fn access(self) -> ShellAccessMode {
        match self {
            Self::HiddenReadWrite => ShellAccessMode::ReadWrite,
            Self::Visible(access) => access,
        }
    }

    fn display_label(self) -> Option<&'static str> {
        match self {
            Self::HiddenReadWrite => None,
            Self::Visible(access) => Some(access.display_label()),
        }
    }
}

/// Build the provider-owned display descriptor published as the first progress
/// event after `tool.started`.
pub(crate) fn initial_display(
    arguments: &CborValue,
    command_mode: ShellCommandMode,
) -> ToolUseState {
    let command = argument_text(arguments, "command").unwrap_or_default();
    ToolUseState {
        args: command_display_args(&command),
        mode: command_mode.display_label().unwrap_or_default().to_owned(),
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        payload: command_display_payload(&command),
        ..Default::default()
    }
}

/// Execute a `shell` tool call.///
/// **Process outcome semantics.** Commands that start successfully always
/// produce `ToolResult`, even when they exit non-zero, time out, or terminate
/// by signal. Those expected process outcomes are represented by structured
/// result fields such as `status`, `timed_out`, `signal`, and
/// `termination_reason`; true invocation/config/start errors remain
/// `ToolError`.
#[derive(Debug)]
pub(crate) enum CommandOutcome {
    Finished(Box<ToolOutput>),
    Cancelled,
}

pub(crate) fn run_command_cancellable(
    call_id: &str,
    arguments: &CborValue,
    shell_config: &ShellConfig,
    command_mode: ShellCommandMode,
    enforce_ro_bind: bool,
    cancel_rx: Option<mpsc::Receiver<()>>,
    world: &mut ShellWorld,
) -> Result<CommandOutcome, ToolFailure> {
    if let Some(outcome) = world.replay_shell_outcome()? {
        return replay_shell_outcome(call_id, outcome, arguments, cancel_rx);
    }

    let started = std::time::Instant::now();
    let outcome = run_command_live(
        arguments,
        shell_config,
        command_mode,
        enforce_ro_bind,
        cancel_rx,
    )?;
    let elapsed_ms = elapsed_millis(started.elapsed());
    let recorded = match &outcome {
        CommandOutcome::Finished(output) => WorldShellOutcome::Finished {
            result: output.result.clone(),
            display: Box::new(output.display.clone()),
            elapsed_ms,
        },
        CommandOutcome::Cancelled => WorldShellOutcome::Cancelled,
    };
    world.record_shell_outcome(recorded);
    Ok(outcome)
}

pub(crate) fn run_command_live(
    arguments: &CborValue,
    shell_config: &ShellConfig,
    command_mode: ShellCommandMode,
    enforce_ro_bind: bool,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<CommandOutcome, ToolFailure> {
    let command = argument_text(arguments, "command").map_err(ToolFailure::from)?;
    let cwd = optional_argument_text(arguments, "cwd").map_err(ToolFailure::from)?;
    let display_mode = command_mode.display_label().unwrap_or_default();
    let display_args = command_display_args(&command);
    let display_payload = command_display_payload(&command);
    let timeout_secs = parse_timeout_secs(arguments).map_err(|message| {
        ToolFailure::from(message)
            .with_args(display_args.clone())
            .with_mode(display_mode)
            .with_payload(display_payload.clone())
    })?;
    let timeout = std::time::Duration::from_secs(timeout_secs);

    debug!(command = %command, cwd = ?cwd, timeout_secs, "starting shell command");
    let child = shell_config
        .spawn_isolated(
            &command,
            cwd.as_deref(),
            command_mode.access() == ShellAccessMode::ReadOnly,
            enforce_ro_bind,
        )
        .map_err(|error| {
            ToolFailure::from(format!("failed to start shell command: {error}"))
                .with_args(display_args.clone())
                .with_mode(display_mode)
                .with_payload(display_payload.clone())
                .with_details(command_details_value(CommandDetails {
                    status: None,
                    signal: None,
                    timed_out: false,
                    duration_seconds: None,
                    termination_reason: "start_error",
                    total_lines: None,
                    total_bytes: None,
                    output: String::new(),
                    truncated: false,
                    valid_utf8: true,
                }))
        })?;

    let child_id = child.id();
    debug!(child_id, "shell command spawned");
    let started = std::time::Instant::now();
    let wait = wait_with_timeout(child, timeout, cancel_rx);
    let elapsed = started.elapsed();
    let duration_seconds =
        if std::time::Duration::from_secs(SLOW_COMMAND_EXEC_TIME_THRESHOLD_SECS) < elapsed {
            Some(elapsed.as_secs_f64().ceil() as u64)
        } else {
            None
        };

    let status_code = wait.status_code;
    let signal = wait.signal;
    let success = wait.success;

    if wait.cancelled {
        debug!(child_id, duration_seconds = ?duration_seconds, "shell command cancelled");
        return Ok(CommandOutcome::Cancelled);
    }

    let output_trunc = wait.output.truncate();
    let combined = output_trunc.content.clone();

    let result = command_details_value(CommandDetails {
        status: status_code,
        signal,
        timed_out: wait.timed_out,
        duration_seconds,
        termination_reason: wait.termination_reason,
        total_lines: output_trunc
            .was_truncated
            .then_some(output_trunc.total_lines),
        total_bytes: output_trunc
            .was_truncated
            .then_some(output_trunc.total_bytes),
        output: output_trunc.content,
        truncated: output_trunc.was_truncated,
        valid_utf8: !wait.had_invalid_utf8,
    });

    let mut display = if success {
        ok_display(display_args)
    } else {
        let exit_label = status_code
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let status_text = if wait.timed_out {
            "timeout".to_owned()
        } else if let Some(signal) = signal {
            format!("signal {signal}")
        } else {
            exit_label
        };
        ToolUseState {
            args: display_args,
            status: ToolUseStatus::Error,
            status_text,
            ..Default::default()
        }
    };
    display.mode = display_mode.to_owned();
    display.payload = display_payload;
    display.stats = text_stats(&combined);
    debug!(
        child_id,
        status_code = ?wait.status_code,
        signal = ?wait.signal,
        timed_out = wait.timed_out,
        duration_seconds = ?duration_seconds,
        "shell command finished"
    );
    Ok(CommandOutcome::Finished(Box::new(ToolOutput {
        result,
        display,
    })))
}

fn replay_shell_outcome(
    key: &str,
    outcome: WorldShellOutcome,
    arguments: &CborValue,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<CommandOutcome, ToolFailure> {
    match outcome {
        WorldShellOutcome::Finished {
            result,
            display,
            elapsed_ms,
        } => {
            if cancel_rx.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
                return Err(ToolFailure::new(format!(
                    "vcr replay for {key} expected finished shell call but cancellation was requested"
                )));
            }
            sleep_for_replay_elapsed(elapsed_ms);
            if cancel_rx.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
                return Err(ToolFailure::new(format!(
                    "vcr replay for {key} expected finished shell call but cancellation was requested"
                )));
            }
            Ok(CommandOutcome::Finished(Box::new(ToolOutput {
                result,
                display: *display,
            })))
        }
        WorldShellOutcome::Cancelled => {
            let timeout = std::time::Duration::from_secs(
                parse_timeout_secs(arguments).map_err(ToolFailure::from)?,
            );
            let Some(cancel_rx) = cancel_rx else {
                return Err(ToolFailure::new(format!(
                    "vcr replay for {key} expected shell cancellation but call is not cancellable"
                )));
            };
            match cancel_rx.recv_timeout(timeout) {
                Ok(()) => Ok(CommandOutcome::Cancelled),
                Err(mpsc::RecvTimeoutError::Timeout) => Err(ToolFailure::new(format!(
                    "vcr replay for {key} expected shell cancellation before timeout"
                ))),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(ToolFailure::new(format!(
                    "vcr replay for {key} expected shell cancellation but cancellation channel closed"
                ))),
            }
        }
    }
}

fn elapsed_millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn sleep_for_replay_elapsed(elapsed_ms: u64) {
    if elapsed_ms == 0 {
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(
        elapsed_ms.div_ceil(VCR_REPLAY_SPEEDUP),
    ));
}

fn parse_timeout_secs(arguments: &CborValue) -> Result<u64, String> {
    let Some(timeout) = optional_argument_int_strict(arguments, "timeout")? else {
        return Ok(DEFAULT_TIMEOUT_SECS);
    };
    if timeout < 0 {
        return Err("argument `timeout` must be non-negative".to_owned());
    }
    Ok(timeout as u64)
}

/// Run a user-initiated `!`/`!!` shell command, streaming stdout and
/// stderr back as `ShellCommandProgress` chunks while they arrive and
/// emitting `ShellCommandFinished` with the full (truncated-tail)
/// output when the child exits.
pub(crate) fn dispatch_user_shell_command(
    cmd: tau_proto::UiShellCommand,
    shell_config: ShellConfig,
    tx: &mpsc::Sender<HarnessInputMessage>,
) {
    let child = match shell_config.spawn_isolated(&cmd.command, None, false, false) {
        Ok(child) => child,
        Err(err) => {
            send_user_shell_finished(
                cmd,
                format!("failed to start shell command: {err}"),
                None,
                false,
                tx,
            );
            return;
        }
    };

    #[cfg(unix)]
    dispatch_user_shell_command_unix(cmd, child, shell_config.user_command_timeout_secs, tx);
    #[cfg(not(unix))]
    dispatch_user_shell_command_blocking(cmd, child, shell_config.user_command_timeout_secs, tx);
}

fn send_user_shell_finished(
    cmd: tau_proto::UiShellCommand,
    output: String,
    exit_code: Option<i32>,
    cancelled: bool,
    tx: &mpsc::Sender<HarnessInputMessage>,
) {
    let _ = tx.send(HarnessInputMessage::emit(Event::ShellCommandFinished(
        tau_proto::ShellCommandFinished {
            command_id: cmd.command_id,
            session_id: cmd.session_id,
            command: cmd.command,
            include_in_context: cmd.include_in_context,
            target_agent_id: cmd.target_agent_id,
            output,
            exit_code,
            cancelled,
        },
    )));
}

#[derive(Default)]
struct UserStreamCapture {
    captured: String,
    clipped: bool,
    progress_bytes: usize,
    progress_clipped: bool,
}

impl UserStreamCapture {
    fn push_chunk(&mut self, chunk: &str) {
        if self.captured.len() < MAX_OUTPUT_BYTES {
            let remaining = MAX_OUTPUT_BYTES - self.captured.len();
            let mut end = remaining.min(chunk.len());
            while !chunk.is_char_boundary(end) {
                end -= 1;
            }
            self.captured.push_str(&chunk[..end]);
            self.clipped |= end < chunk.len();
        } else {
            self.clipped = true;
        }
    }

    fn progress_chunk(&mut self, chunk: &str) -> Option<String> {
        if self.progress_clipped {
            return None;
        }
        if self.progress_bytes < MAX_OUTPUT_BYTES {
            let remaining = MAX_OUTPUT_BYTES - self.progress_bytes;
            let mut end = remaining.min(chunk.len());
            while !chunk.is_char_boundary(end) {
                end -= 1;
            }
            self.progress_bytes += end;
            if end == chunk.len() {
                return Some(chunk.to_owned());
            }
        }
        self.progress_clipped = true;
        Some(USER_OUTPUT_TRUNCATED_MARKER.to_owned())
    }
}

fn merged_user_shell_output(
    stdout: UserStreamCapture,
    stderr: UserStreamCapture,
    status_note: Option<String>,
) -> String {
    let mut merged = stdout.captured;
    if !stderr.captured.is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str("[stderr]\n");
        merged.push_str(&stderr.captured);
    }
    if stdout.clipped || stderr.clipped {
        append_guaranteed_output_truncated_marker(&mut merged);
    }
    if let Some(note) = status_note {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(&note);
    }
    crate::truncate::truncate_tail(&merged).content
}

#[cfg(unix)]
#[cfg(unix)]
const USER_SHELL_READ_CHUNK_BYTES: usize = 8192;
#[cfg(unix)]
const USER_SHELL_DRAIN_AFTER_DONE: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(unix)]
fn set_user_shell_nonblocking(fd: std::os::fd::RawFd) {
    #[allow(unsafe_code)]
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if 0 <= flags {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

#[cfg(unix)]
fn read_available_user_shell<R: std::io::Read>(
    pipe: &mut Option<R>,
    stream: tau_proto::ShellStream,
    command_id: &tau_proto::ShellCommandId,
    target_agent_id: &Option<tau_proto::AgentId>,
    capture: &mut UserStreamCapture,
    tx: &mpsc::Sender<HarnessInputMessage>,
) {
    let Some(pipe_ref) = pipe.as_mut() else {
        return;
    };

    let mut close_pipe = false;
    let mut buf = [0u8; USER_SHELL_READ_CHUNK_BYTES];
    loop {
        match pipe_ref.read(&mut buf) {
            Ok(0) => {
                close_pipe = true;
                break;
            }
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                capture.push_chunk(&chunk);
                if let Some(chunk) = capture.progress_chunk(&chunk) {
                    let _ = tx.send(HarnessInputMessage::emit(Event::ShellCommandProgress(
                        tau_proto::ShellCommandProgress {
                            command_id: command_id.clone(),
                            stream,
                            chunk,
                            target_agent_id: target_agent_id.clone(),
                        },
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => {
                close_pipe = true;
                break;
            }
        }
    }
    if close_pipe {
        *pipe = None;
    }
}

#[cfg(unix)]
fn collect_user_shell_status(
    status_rx: &mpsc::Receiver<Option<std::process::ExitStatus>>,
    status: &mut Option<std::process::ExitStatus>,
    wait_failed: &mut bool,
) -> bool {
    use std::sync::mpsc::TryRecvError;

    if status.is_some() || *wait_failed {
        return true;
    }
    match status_rx.try_recv() {
        Ok(Some(received)) => {
            *status = Some(received);
            true
        }
        Ok(None) | Err(TryRecvError::Disconnected) => {
            *wait_failed = true;
            true
        }
        Err(TryRecvError::Empty) => false,
    }
}

#[cfg(unix)]
fn user_shell_poll_timeout_ms(deadline: std::time::Instant) -> i32 {
    let now = std::time::Instant::now();
    if deadline <= now {
        return 0;
    }
    let remaining = deadline - now;
    i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX)
}

#[cfg(unix)]
fn drain_user_shell_wake_fd(wake_read: &std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;

    let mut buf = [0u8; 16];
    loop {
        #[allow(unsafe_code)]
        let n = unsafe {
            libc::read(
                wake_read.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if 0 < n {
            continue;
        }
        break;
    }
}

#[cfg(unix)]
fn push_user_shell_poll_fd(poll_fds: &mut Vec<libc::pollfd>, fd: std::os::fd::RawFd) {
    poll_fds.push(libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    });
}

#[cfg(unix)]
fn user_shell_poll_fds(
    stdout_pipe: Option<&std::process::ChildStdout>,
    stderr_pipe: Option<&std::process::ChildStderr>,
    wake_read: Option<&std::os::fd::OwnedFd>,
) -> Vec<libc::pollfd> {
    use std::os::fd::AsRawFd;

    let mut poll_fds = Vec::new();
    if let Some(pipe) = stdout_pipe {
        push_user_shell_poll_fd(&mut poll_fds, pipe.as_raw_fd());
    }
    if let Some(pipe) = stderr_pipe {
        push_user_shell_poll_fd(&mut poll_fds, pipe.as_raw_fd());
    }
    if let Some(wake_read) = wake_read {
        push_user_shell_poll_fd(&mut poll_fds, wake_read.as_raw_fd());
    }
    poll_fds
}

#[cfg(unix)]
fn dispatch_user_shell_command_unix(
    cmd: tau_proto::UiShellCommand,
    mut child: std::process::Child,
    timeout_secs: u64,
    tx: &mpsc::Sender<HarnessInputMessage>,
) {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let timeout = std::time::Duration::from_secs(timeout_secs);
    let pid = child.id();
    debug!(
        pid,
        timeout_ms = timeout.as_millis(),
        "waiting for user shell child"
    );

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    if let Some(pipe) = stdout_pipe.as_ref() {
        set_user_shell_nonblocking(pipe.as_raw_fd());
    }
    if let Some(pipe) = stderr_pipe.as_ref() {
        set_user_shell_nonblocking(pipe.as_raw_fd());
    }

    let mut wake_fds = [0; 2];
    #[allow(unsafe_code)]
    let wake_pipe_ok = unsafe { libc::pipe(wake_fds.as_mut_ptr()) == 0 };
    let (wake_read, wake_write) = if wake_pipe_ok {
        #[allow(unsafe_code)]
        unsafe {
            (
                Some(OwnedFd::from_raw_fd(wake_fds[0])),
                Some(OwnedFd::from_raw_fd(wake_fds[1])),
            )
        }
    } else {
        (None, None)
    };
    if let Some(wake_read) = wake_read.as_ref() {
        set_user_shell_nonblocking(wake_read.as_raw_fd());
    }
    let waiter_wake_read = wake_read.as_ref().and_then(|wake_read| {
        #[allow(unsafe_code)]
        let fd = unsafe { libc::dup(wake_read.as_raw_fd()) };
        if 0 <= fd {
            #[allow(unsafe_code)]
            unsafe {
                Some(OwnedFd::from_raw_fd(fd))
            }
        } else {
            None
        }
    });

    let (status_tx, status_rx) = mpsc::channel::<Option<std::process::ExitStatus>>();
    let _waiter = std::thread::spawn(move || {
        let _wake_read_guard = waiter_wake_read;
        let status = child.wait().ok();
        debug!(pid, status = ?status, "user shell child waiter finished");
        let _ = status_tx.send(status);
        if let Some(wake_write) = wake_write {
            let byte = [1u8];
            #[allow(unsafe_code)]
            unsafe {
                let _ = libc::write(
                    wake_write.as_raw_fd(),
                    byte.as_ptr().cast::<libc::c_void>(),
                    byte.len(),
                );
            }
        }
    });

    let mut stdout = UserStreamCapture::default();
    let mut stderr = UserStreamCapture::default();
    let mut status = None;
    let mut wait_failed = false;
    let mut timed_out = false;
    let deadline = std::time::Instant::now() + timeout;

    loop {
        read_available_user_shell(
            &mut stdout_pipe,
            tau_proto::ShellStream::Stdout,
            &cmd.command_id,
            &cmd.target_agent_id,
            &mut stdout,
            tx,
        );
        read_available_user_shell(
            &mut stderr_pipe,
            tau_proto::ShellStream::Stderr,
            &cmd.command_id,
            &cmd.target_agent_id,
            &mut stderr,
            tx,
        );
        if collect_user_shell_status(&status_rx, &mut status, &mut wait_failed) {
            break;
        }
        let now = std::time::Instant::now();
        if deadline <= now {
            timed_out = true;
            kill_process_group_by_pid(pid);
            break;
        }

        let mut poll_fds = user_shell_poll_fds(
            stdout_pipe.as_ref(),
            stderr_pipe.as_ref(),
            wake_read.as_ref(),
        );

        if poll_fds.is_empty() {
            let sleep_for = (deadline - now).min(std::time::Duration::from_millis(25));
            std::thread::sleep(sleep_for);
            continue;
        }

        #[allow(unsafe_code)]
        unsafe {
            let _ = libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as libc::nfds_t,
                user_shell_poll_timeout_ms(deadline),
            );
        }
        if let Some(wake_read) = wake_read.as_ref() {
            drain_user_shell_wake_fd(wake_read);
        }
    }

    let drain_deadline = std::time::Instant::now() + USER_SHELL_DRAIN_AFTER_DONE;
    loop {
        read_available_user_shell(
            &mut stdout_pipe,
            tau_proto::ShellStream::Stdout,
            &cmd.command_id,
            &cmd.target_agent_id,
            &mut stdout,
            tx,
        );
        read_available_user_shell(
            &mut stderr_pipe,
            tau_proto::ShellStream::Stderr,
            &cmd.command_id,
            &cmd.target_agent_id,
            &mut stderr,
            tx,
        );
        let _ = collect_user_shell_status(&status_rx, &mut status, &mut wait_failed);
        if (stdout_pipe.is_none() && stderr_pipe.is_none())
            || drain_deadline <= std::time::Instant::now()
        {
            break;
        }

        let mut poll_fds = user_shell_poll_fds(stdout_pipe.as_ref(), stderr_pipe.as_ref(), None);
        if poll_fds.is_empty() {
            break;
        }
        #[allow(unsafe_code)]
        unsafe {
            let _ = libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as libc::nfds_t,
                user_shell_poll_timeout_ms(drain_deadline),
            );
        }
    }

    let exit_code = status.as_ref().and_then(|status| status.code());
    let status_note = if timed_out {
        Some(format!("command killed after {timeout_secs}s timeout"))
    } else if wait_failed {
        Some("wait failed".to_owned())
    } else {
        None
    };
    let output = merged_user_shell_output(stdout, stderr, status_note);
    send_user_shell_finished(cmd, output, exit_code, timed_out, tx);
}

#[cfg(not(unix))]
fn dispatch_user_shell_command_blocking(
    cmd: tau_proto::UiShellCommand,
    mut child: std::process::Child,
    timeout_secs: u64,
    tx: &mpsc::Sender<HarnessInputMessage>,
) {
    use std::io::Read;

    fn pump<R: Read + Send + 'static>(
        mut pipe: R,
        stream: tau_proto::ShellStream,
        command_id: tau_proto::ShellCommandId,
        target_agent_id: Option<tau_proto::AgentId>,
        tx: mpsc::Sender<HarnessInputMessage>,
    ) -> std::thread::JoinHandle<UserStreamCapture> {
        std::thread::spawn(move || {
            let mut capture = UserStreamCapture::default();
            let mut buf = [0u8; 4096];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                        capture.push_chunk(&chunk);
                        if let Some(chunk) = capture.progress_chunk(&chunk) {
                            let _ = tx.send(HarnessInputMessage::emit(
                                Event::ShellCommandProgress(tau_proto::ShellCommandProgress {
                                    command_id: command_id.clone(),
                                    stream,
                                    chunk,
                                    target_agent_id: target_agent_id.clone(),
                                }),
                            ));
                        }
                    }
                }
            }
            capture
        })
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = stdout_pipe.map(|p| {
        pump(
            p,
            tau_proto::ShellStream::Stdout,
            cmd.command_id.clone(),
            cmd.target_agent_id.clone(),
            tx.clone(),
        )
    });
    let stderr_handle = stderr_pipe.map(|p| {
        pump(
            p,
            tau_proto::ShellStream::Stderr,
            cmd.command_id.clone(),
            cmd.target_agent_id.clone(),
            tx.clone(),
        )
    });

    let timeout = std::time::Duration::from_secs(timeout_secs);
    let pid = child.id();
    debug!(
        pid,
        timeout_ms = timeout.as_millis(),
        "waiting for user shell child"
    );
    let (done_tx, done_rx) = mpsc::channel::<Option<std::process::ExitStatus>>();
    let waiter = std::thread::spawn(move || {
        let status = child.wait().ok();
        let _ = done_tx.send(status);
    });

    let (exit_code, status_note, cancelled) = match done_rx.recv_timeout(timeout) {
        Ok(Some(status)) => (status.code(), None, false),
        Ok(None) => (None, Some("wait failed".to_owned()), false),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            #[cfg(unix)]
            kill_process_group_by_pid(pid);
            let status = done_rx.recv().ok().flatten();
            let _ = waiter.join();
            (
                status.and_then(|s| s.code()),
                Some(format!("command killed after {timeout_secs}s timeout")),
                true,
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            (None, Some("waiter thread vanished".to_owned()), false)
        }
    };

    let stdout = stdout_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let output = merged_user_shell_output(stdout, stderr, status_note);
    send_user_shell_finished(cmd, output, exit_code, cancelled, tx);
}

fn append_guaranteed_output_truncated_marker(output: &mut String) {
    let separator = if output.is_empty() { "" } else { "\n" };
    let tail_len = separator.len() + USER_OUTPUT_TRUNCATED_MARKER.len();
    if output.len().saturating_add(tail_len) <= MAX_OUTPUT_BYTES {
        output.push_str(separator);
        output.push_str(USER_OUTPUT_TRUNCATED_MARKER);
        return;
    }

    let budget = MAX_OUTPUT_BYTES.saturating_sub(tail_len);
    let mut end = budget.min(output.len());
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(USER_OUTPUT_TRUNCATED_MARKER);
}

#[cfg(unix)]
const SHELL_WAIT_READ_CHUNK_BYTES: usize = 8192;
#[cfg(unix)]
const SHELL_WAIT_DRAIN_AFTER_DONE: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(unix)]
fn shell_wait_set_nonblocking(fd: std::os::fd::RawFd) {
    #[allow(unsafe_code)]
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if 0 <= flags {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

#[cfg(unix)]
fn shell_wait_read_available<R: std::io::Read>(
    pipe: &mut Option<R>,
    stream: OutputStream,
    capture: &mut CapturedOutput,
) {
    let Some(pipe_ref) = pipe.as_mut() else {
        return;
    };

    let mut close_pipe = false;
    let mut buf = [0u8; SHELL_WAIT_READ_CHUNK_BYTES];
    loop {
        match pipe_ref.read(&mut buf) {
            Ok(0) => {
                close_pipe = true;
                break;
            }
            Ok(n) => capture.push_bytes(stream, &buf[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => {
                close_pipe = true;
                break;
            }
        }
    }
    if close_pipe {
        *pipe = None;
    }
}

#[cfg(unix)]
fn shell_wait_collect_status(
    status_rx: &mpsc::Receiver<Option<std::process::ExitStatus>>,
    status: &mut Option<std::process::ExitStatus>,
) -> bool {
    use std::sync::mpsc::TryRecvError;

    if status.is_some() {
        return true;
    }
    match status_rx.try_recv() {
        Ok(received) => {
            *status = received;
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => true,
    }
}

#[cfg(unix)]
fn shell_wait_poll_timeout_ms(deadline: std::time::Instant) -> i32 {
    let now = std::time::Instant::now();
    if deadline <= now {
        return 0;
    }
    let remaining = deadline - now;
    i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX)
}

#[cfg(unix)]
fn shell_wait_dup_owned_fd(fd: &std::os::fd::OwnedFd) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[allow(unsafe_code)]
    let duplicated = unsafe { libc::dup(fd.as_raw_fd()) };
    if duplicated < 0 {
        return None;
    }
    #[allow(unsafe_code)]
    unsafe {
        Some(OwnedFd::from_raw_fd(duplicated))
    }
}

#[cfg(unix)]
fn shell_wait_write_wake_byte(fd: &std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;

    let byte = [1u8];
    #[allow(unsafe_code)]
    unsafe {
        let _ = libc::write(
            fd.as_raw_fd(),
            byte.as_ptr().cast::<libc::c_void>(),
            byte.len(),
        );
    }
}

#[cfg(unix)]
fn shell_wait_drain_wake_fd(wake_read: &std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;

    let mut buf = [0u8; 16];
    loop {
        #[allow(unsafe_code)]
        let n = unsafe {
            libc::read(
                wake_read.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if 0 < n {
            continue;
        }
        break;
    }
}

#[cfg(unix)]
fn shell_wait_poll_fds_until(fds: &mut [libc::pollfd], deadline: std::time::Instant) {
    #[allow(unsafe_code)]
    unsafe {
        let _ = libc::poll(
            fds.as_mut_ptr(),
            fds.len() as libc::nfds_t,
            shell_wait_poll_timeout_ms(deadline),
        );
    }
}

#[cfg(unix)]
struct ShellWaitWakePipe {
    /// Read side observed by the foreground polling loop.
    read: std::os::fd::OwnedFd,
    /// Write side used by the child waiter thread.
    write: std::os::fd::OwnedFd,
}

#[cfg(unix)]
impl ShellWaitWakePipe {
    fn open() -> Option<Self> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let mut wake_fds = [0; 2];
        #[allow(unsafe_code)]
        let wake_pipe_ok = unsafe { libc::pipe(wake_fds.as_mut_ptr()) == 0 };
        if !wake_pipe_ok {
            return None;
        }

        #[allow(unsafe_code)]
        let (read, write) = unsafe {
            (
                OwnedFd::from_raw_fd(wake_fds[0]),
                OwnedFd::from_raw_fd(wake_fds[1]),
            )
        };
        shell_wait_set_nonblocking(read.as_raw_fd());
        Some(Self { read, write })
    }
}

#[cfg(unix)]
enum ShellWaitPoll {
    Continue,
    Finished,
    Cancelled,
    TimedOut,
}

#[cfg(unix)]
struct ShellWaitState {
    /// Child process id used as process-group id for termination.
    pid: u32,
    /// Nonblocking stdout pipe owned by the foreground wait loop.
    stdout_pipe: Option<std::process::ChildStdout>,
    /// Nonblocking stderr pipe owned by the foreground wait loop.
    stderr_pipe: Option<std::process::ChildStderr>,
    /// Wake fd signalled when the child exits or cancellation arrives.
    wake_read: Option<std::os::fd::OwnedFd>,
    /// Receiver for the child waiter thread's exit status.
    status_rx: mpsc::Receiver<Option<std::process::ExitStatus>>,
    /// Cross-thread cancellation flag set by the cancellation waiter.
    cancelled_by_request: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(unix)]
impl ShellWaitState {
    fn start(mut child: std::process::Child, cancel_rx: Option<mpsc::Receiver<()>>) -> Self {
        use std::os::fd::AsRawFd;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let pid = child.id();
        debug!(
            pid,
            cancel_enabled = cancel_rx.is_some(),
            "starting shell wait state"
        );

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        if let Some(pipe) = stdout_pipe.as_ref() {
            shell_wait_set_nonblocking(pipe.as_raw_fd());
        }
        if let Some(pipe) = stderr_pipe.as_ref() {
            shell_wait_set_nonblocking(pipe.as_raw_fd());
        }

        let wake_pipe = ShellWaitWakePipe::open();
        let cancel_wake_write = wake_pipe
            .as_ref()
            .and_then(|wake_pipe| shell_wait_dup_owned_fd(&wake_pipe.write));
        let waiter_wake_read = wake_pipe
            .as_ref()
            .and_then(|wake_pipe| shell_wait_dup_owned_fd(&wake_pipe.read));
        let cancelled_by_request = Arc::new(AtomicBool::new(false));
        spawn_shell_cancel_waiter(
            pid,
            cancel_rx,
            Arc::clone(&cancelled_by_request),
            cancel_wake_write,
        );

        let (wake_read, wake_write) = wake_pipe
            .map(|wake_pipe| (Some(wake_pipe.read), Some(wake_pipe.write)))
            .unwrap_or((None, None));
        let status_rx = spawn_shell_child_waiter(pid, child, wake_write, waiter_wake_read);
        Self {
            pid,
            stdout_pipe,
            stderr_pipe,
            wake_read,
            status_rx,
            cancelled_by_request,
        }
    }

    fn wait(mut self, timeout: std::time::Duration) -> WaitResult {
        debug!(
            pid = self.pid,
            timeout_ms = timeout.as_millis(),
            "waiting for shell child"
        );
        let mut output = CapturedOutput::default();
        let mut status = None;
        let mut timed_out = false;
        let mut cancelled = false;
        let deadline = std::time::Instant::now() + timeout;

        loop {
            match self.poll_until_terminal(&mut output, &mut status, deadline) {
                ShellWaitPoll::Continue => {}
                ShellWaitPoll::Finished => break,
                ShellWaitPoll::Cancelled => {
                    cancelled = true;
                    break;
                }
                ShellWaitPoll::TimedOut => {
                    timed_out = true;
                    break;
                }
            }
        }

        self.drain_after_terminal(&mut output, &mut status);
        output.finish();
        debug!(pid = self.pid, status = ?status, timed_out, cancelled, "shell wait completed");
        wait_result_from_parts(status, timed_out, cancelled, output)
    }

    fn poll_until_terminal(
        &mut self,
        output: &mut CapturedOutput,
        status: &mut Option<std::process::ExitStatus>,
        deadline: std::time::Instant,
    ) -> ShellWaitPoll {
        use std::sync::atomic::Ordering;

        self.read_available_output(output);
        if shell_wait_collect_status(&self.status_rx, status) {
            debug!(pid = self.pid, status = ?status, "shell wait loop observed child status");
            return ShellWaitPoll::Finished;
        }
        if self.cancelled_by_request.load(Ordering::SeqCst) {
            debug!(
                pid = self.pid,
                "shell wait loop observed cancellation; killing process group"
            );
            kill_process_group_by_pid(self.pid);
            return ShellWaitPoll::Cancelled;
        }

        let now = std::time::Instant::now();
        if deadline <= now {
            debug!(
                pid = self.pid,
                "shell wait loop timed out; killing process group"
            );
            kill_process_group_by_pid(self.pid);
            return ShellWaitPoll::TimedOut;
        }

        self.wait_for_io_or_wake(deadline, now);
        ShellWaitPoll::Continue
    }

    fn drain_after_terminal(
        &mut self,
        output: &mut CapturedOutput,
        status: &mut Option<std::process::ExitStatus>,
    ) {
        let drain_deadline = std::time::Instant::now() + SHELL_WAIT_DRAIN_AFTER_DONE;
        loop {
            self.read_available_output(output);
            let _ = shell_wait_collect_status(&self.status_rx, status);
            if self.stdout_pipe.is_none() && self.stderr_pipe.is_none() {
                trace!(pid = self.pid, "shell output drain completed");
                break;
            }
            if drain_deadline <= std::time::Instant::now() {
                trace!(pid = self.pid, "shell output drain deadline reached");
                break;
            }

            let mut poll_fds = self.output_poll_fds();
            if poll_fds.is_empty() {
                break;
            }
            shell_wait_poll_fds_until(&mut poll_fds, drain_deadline);
        }
    }

    fn read_available_output(&mut self, output: &mut CapturedOutput) {
        shell_wait_read_available(&mut self.stdout_pipe, OutputStream::Stdout, output);
        shell_wait_read_available(&mut self.stderr_pipe, OutputStream::Stderr, output);
    }

    fn wait_for_io_or_wake(&self, deadline: std::time::Instant, now: std::time::Instant) {
        let mut poll_fds = self.output_and_wake_poll_fds();
        if poll_fds.is_empty() {
            let sleep_for = (deadline - now).min(std::time::Duration::from_millis(25));
            std::thread::sleep(sleep_for);
            return;
        }

        shell_wait_poll_fds_until(&mut poll_fds, deadline);
        if let Some(wake_read) = self.wake_read.as_ref() {
            shell_wait_drain_wake_fd(wake_read);
        }
    }

    fn output_poll_fds(&self) -> Vec<libc::pollfd> {
        use std::os::fd::AsRawFd;

        let mut poll_fds = Vec::new();
        if let Some(pipe) = self.stdout_pipe.as_ref() {
            poll_fds.push(libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        if let Some(pipe) = self.stderr_pipe.as_ref() {
            poll_fds.push(libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        poll_fds
    }

    fn output_and_wake_poll_fds(&self) -> Vec<libc::pollfd> {
        use std::os::fd::AsRawFd;

        let mut poll_fds = self.output_poll_fds();
        if let Some(wake_read) = self.wake_read.as_ref() {
            poll_fds.push(libc::pollfd {
                fd: wake_read.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            });
        }
        poll_fds
    }
}

#[cfg(unix)]
fn spawn_shell_cancel_waiter(
    pid: u32,
    cancel_rx: Option<mpsc::Receiver<()>>,
    cancelled_by_request: std::sync::Arc<std::sync::atomic::AtomicBool>,
    cancel_wake_write: Option<std::os::fd::OwnedFd>,
) {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    let Some(cancel_rx) = cancel_rx else {
        return;
    };
    let cancelled_by_request = Arc::clone(&cancelled_by_request);
    std::thread::spawn(move || {
        if cancel_rx.recv().is_ok() {
            debug!(pid, "shell cancellation signal received");
            cancelled_by_request.store(true, Ordering::SeqCst);
            if let Some(cancel_wake_write) = cancel_wake_write {
                trace!(pid, "waking shell wait loop after cancellation");
                shell_wait_write_wake_byte(&cancel_wake_write);
            }
        }
    });
}

#[cfg(unix)]
fn spawn_shell_child_waiter(
    pid: u32,
    mut child: std::process::Child,
    wake_write: Option<std::os::fd::OwnedFd>,
    waiter_wake_read: Option<std::os::fd::OwnedFd>,
) -> mpsc::Receiver<Option<std::process::ExitStatus>> {
    let (status_tx, status_rx) = mpsc::channel::<Option<std::process::ExitStatus>>();
    let _waiter = std::thread::spawn(move || {
        // Keep a duplicate read end alive until the waiter publishes status and
        // writes the final wake byte. Otherwise a foreground loop that returns
        // early after timeout/cancellation could drop the last reader first,
        // turning this best-effort wake into an avoidable no-reader write.
        let _wake_read_guard = waiter_wake_read;
        let status = child.wait().ok();
        debug!(pid, status = ?status, "shell child waiter finished");
        let _ = status_tx.send(status);
        if let Some(wake_write) = wake_write {
            shell_wait_write_wake_byte(&wake_write);
        }
    });
    status_rx
}

#[cfg(unix)]
fn wait_with_timeout(
    child: std::process::Child,
    timeout: std::time::Duration,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> WaitResult {
    // On Unix the shell tool must not wait for stdout/stderr EOF: background
    // or detached descendants can inherit those pipe write ends long after the
    // foreground shell exits or is killed. The wait state therefore polls
    // nonblocking pipes and an internal child-exit wake pipe together, then
    // returns after foreground exit or timeout with only a brief nonblocking
    // drain.
    ShellWaitState::start(child, cancel_rx).wait(timeout)
}

/// Wait for a child process with a timeout, preserving output even when
/// the timeout is reached.
///
/// Non-Unix keeps the older blocking-pipe fallback. The crate's process-group
/// isolation is Unix-only, so the hard timeout/read-loop guarantees are
/// provided by the Unix implementation above.
#[cfg(not(unix))]
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: std::time::Duration,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> WaitResult {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_handle = std::thread::spawn(move || read_pipe(stdout_pipe, OutputStream::Stdout));
    let stderr_handle = std::thread::spawn(move || read_pipe(stderr_pipe, OutputStream::Stderr));

    let deadline = std::time::Instant::now() + timeout;
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }

        if cancel_rx.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
            cancelled = true;
            let _ = child.kill();
            break child.wait().ok();
        }

        let now = std::time::Instant::now();
        if deadline <= now {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }

        std::thread::sleep((deadline - now).min(std::time::Duration::from_millis(25)));
    };

    let mut output = CapturedOutput::default();
    let stdout_output = stdout_handle.join().unwrap_or_default();
    output.mark_invalid_utf8(OutputStream::Stdout, stdout_output.had_invalid_utf8);
    for line in stdout_output.lines {
        output.push_line(line.stream, line.content);
    }
    let stderr_output = stderr_handle.join().unwrap_or_default();
    output.mark_invalid_utf8(OutputStream::Stderr, stderr_output.had_invalid_utf8);
    for line in stderr_output.lines {
        output.push_line(line.stream, line.content);
    }
    wait_result_from_parts(status, timed_out, cancelled, output)
}

#[cfg(unix)]
fn exit_status_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;

    status.signal()
}

#[cfg(not(unix))]
fn exit_status_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(unix)]
fn kill_process_group_by_pid(pid: u32) {
    #[allow(unsafe_code)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn wait_result_from_parts(
    status: Option<std::process::ExitStatus>,
    timed_out: bool,
    cancelled: bool,
    output: CapturedOutput,
) -> WaitResult {
    let status_code = status.as_ref().and_then(|status| status.code());
    let signal = status.as_ref().and_then(exit_status_signal);
    let success =
        !timed_out && !cancelled && status.as_ref().is_some_and(|status| status.success());
    let termination_reason = if cancelled {
        "cancelled"
    } else if timed_out {
        "timeout"
    } else if signal.is_some() {
        "signal"
    } else if status.is_some() {
        "exit"
    } else {
        "unknown"
    };

    let had_invalid_utf8 = output.stdout.had_invalid_utf8 || output.stderr.had_invalid_utf8;
    WaitResult {
        status_code,
        signal,
        success,
        output,
        had_invalid_utf8,
        timed_out,
        cancelled,
        termination_reason,
    }
}

struct WaitResult {
    status_code: Option<i32>,
    signal: Option<i32>,
    success: bool,
    output: CapturedOutput,
    had_invalid_utf8: bool,
    timed_out: bool,
    cancelled: bool,
    termination_reason: &'static str,
}

#[derive(Clone, Copy)]
enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    fn prefix(self) -> &'static str {
        match self {
            Self::Stdout => "out",
            Self::Stderr => "err",
        }
    }
}

struct OutputLine {
    stream: OutputStream,
    content: OutputContent,
}

#[derive(Clone)]
enum OutputContent {
    Text {
        text: String,
        ending: Option<LineEndingKind>,
    },
    InvalidUtf8 {
        text: String,
        ending: Option<LineEndingKind>,
    },
    Truncated {
        invalid_utf8: bool,
        ending: Option<LineEndingKind>,
        original_text_bytes: usize,
    },
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum LineEndingKind {
    Lf,
    Crlf,
    Cr,
}

#[derive(Default)]
struct CapturedOutput {
    stdout: StreamDecoder,
    stderr: StreamDecoder,
    head_lines: Vec<OutputLine>,
    tail_lines: Vec<OutputLine>,
    total_lines: usize,
    total_bytes: usize,
    saw_truncated_line: bool,
}

impl CapturedOutput {
    fn push_bytes(&mut self, stream: OutputStream, bytes: &[u8]) {
        let decoder = match stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
        };
        for line in decoder.push_bytes(bytes) {
            self.push_line(stream, line);
        }
    }

    fn push_line(&mut self, stream: OutputStream, content: OutputContent) {
        let separator_bytes = usize::from(self.total_lines != 0);
        self.total_bytes += separator_bytes + formatted_output_line_len(stream, &content);
        if matches!(content, OutputContent::Truncated { .. }) {
            self.saw_truncated_line = true;
        }
        let line = OutputLine { stream, content };
        if self.total_lines < MAX_OUTPUT_LINES / 2 {
            self.head_lines.push(line);
        } else {
            self.tail_lines.push(line);
            if MAX_OUTPUT_LINES / 2 < self.tail_lines.len() {
                self.tail_lines.remove(0);
            }
        }
        self.total_lines += 1;
    }

    fn finish(&mut self) {
        for line in self.stdout.finish() {
            self.push_line(OutputStream::Stdout, line);
        }
        for line in self.stderr.finish() {
            self.push_line(OutputStream::Stderr, line);
        }
    }

    #[cfg(not(unix))]
    fn mark_invalid_utf8(&mut self, stream: OutputStream, had_invalid_utf8: bool) {
        if !had_invalid_utf8 {
            return;
        }
        match stream {
            OutputStream::Stdout => self.stdout.had_invalid_utf8 = true,
            OutputStream::Stderr => self.stderr.had_invalid_utf8 = true,
        }
    }

    fn truncate(&self) -> crate::truncate::Truncated {
        let mut rendered = self
            .head_lines
            .iter()
            .map(render_output_line)
            .collect::<Vec<_>>();
        rendered.extend(self.tail_lines.iter().map(render_output_line));
        let rendered_refs = rendered.iter().map(String::as_str).collect::<Vec<_>>();
        truncate_line_oriented_lines(
            rendered_refs.iter().copied(),
            self.total_lines,
            if self.saw_truncated_line {
                self.total_bytes.max(MAX_OUTPUT_BYTES + 1)
            } else {
                self.total_bytes
            },
        )
    }
}

#[derive(Default)]
struct StreamDecoder {
    pending_utf8: Vec<u8>,
    pending_line: String,
    pending_line_original_bytes: usize,
    pending_line_invalid: bool,
    pending_line_truncated: bool,
    pending_cr: bool,
    had_invalid_utf8: bool,
}

impl StreamDecoder {
    fn push_bytes(&mut self, bytes: &[u8]) -> Vec<OutputContent> {
        if bytes.is_empty() {
            return Vec::new();
        }

        let mut lines = Vec::new();
        let mut merged;
        let mut remaining = if self.pending_utf8.is_empty() {
            bytes
        } else {
            merged = std::mem::take(&mut self.pending_utf8);
            merged.extend_from_slice(bytes);
            &merged
        };

        loop {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    self.push_str(valid, &mut lines);
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if 0 < valid_up_to {
                        self.push_str(
                            std::str::from_utf8(&remaining[..valid_up_to]).unwrap_or(""),
                            &mut lines,
                        );
                    }
                    if let Some(error_len) = error.error_len() {
                        self.flush_pending_cr_as_cr(&mut lines);
                        self.had_invalid_utf8 = true;
                        self.pending_line_invalid = true;
                        if !self.pending_line_truncated {
                            self.push_char('\u{fffd}');
                        }
                        remaining = &remaining[valid_up_to + error_len..];
                    } else {
                        self.flush_pending_cr_as_cr(&mut lines);
                        self.pending_utf8 = remaining[valid_up_to..].to_vec();
                        break;
                    }
                }
            }
        }
        lines
    }

    fn push_str(&mut self, text: &str, lines: &mut Vec<OutputContent>) {
        for ch in text.chars() {
            if self.pending_cr {
                self.pending_cr = false;
                if ch == '\n' {
                    lines.push(self.take_pending_line(Some(LineEndingKind::Crlf)));
                    continue;
                }
                lines.push(self.take_pending_line(Some(LineEndingKind::Cr)));
            }

            match ch {
                '\r' => self.pending_cr = true,
                '\n' => lines.push(self.take_pending_line(Some(LineEndingKind::Lf))),
                _ => self.push_char(ch),
            }
        }
    }

    fn push_char(&mut self, ch: char) {
        let next_len = self
            .pending_line_original_bytes
            .saturating_add(ch.len_utf8());
        self.pending_line_original_bytes = next_len;
        if self.pending_line_truncated {
            return;
        }
        if MAX_CAPTURED_LINE_BYTES < next_len {
            self.pending_line.clear();
            self.pending_line_truncated = true;
            return;
        }
        self.pending_line.push(ch);
    }
    fn finish(&mut self) -> Vec<OutputContent> {
        let mut lines = Vec::new();
        if !self.pending_utf8.is_empty() {
            self.had_invalid_utf8 = true;
            self.pending_utf8.clear();
            self.flush_pending_cr_as_cr(&mut lines);
            self.pending_line_invalid = true;
            if !self.pending_line_truncated {
                self.push_char('\u{fffd}');
            }
        }
        self.flush_pending_cr_as_cr(&mut lines);
        if !self.pending_line.is_empty() || self.pending_line_invalid || self.pending_line_truncated
        {
            lines.push(self.take_pending_line(None));
        }
        lines
    }

    fn flush_pending_cr_as_cr(&mut self, lines: &mut Vec<OutputContent>) {
        if self.pending_cr {
            self.pending_cr = false;
            lines.push(self.take_pending_line(Some(LineEndingKind::Cr)));
        }
    }

    fn take_pending_line(&mut self, ending: Option<LineEndingKind>) -> OutputContent {
        let original_text_bytes = std::mem::take(&mut self.pending_line_original_bytes);
        if std::mem::take(&mut self.pending_line_truncated) {
            let invalid_utf8 = std::mem::take(&mut self.pending_line_invalid);
            self.pending_line.clear();
            return OutputContent::Truncated {
                invalid_utf8,
                ending,
                original_text_bytes,
            };
        }
        if std::mem::take(&mut self.pending_line_invalid) {
            OutputContent::InvalidUtf8 {
                text: std::mem::take(&mut self.pending_line),
                ending,
            }
        } else {
            OutputContent::Text {
                text: std::mem::take(&mut self.pending_line),
                ending,
            }
        }
    }
}

fn render_output_line(line: &OutputLine) -> String {
    let prefix = line.stream.prefix();
    match &line.content {
        OutputContent::Text { text, ending } => {
            format_output_line(prefix, line_ending_marker(*ending), text)
        }
        OutputContent::InvalidUtf8 { text, ending } => {
            let mut markers = vec!["invalid-utf8"];
            if let Some(marker) = line_ending_marker(*ending) {
                markers.push(marker);
            }
            format_output_line(prefix, Some(&markers.join(",")), text)
        }
        OutputContent::Truncated {
            invalid_utf8,
            ending,
            ..
        } => {
            let mut markers = Vec::new();
            if *invalid_utf8 {
                markers.push("invalid-utf8");
            }
            if let Some(marker) = line_ending_marker(*ending) {
                markers.push(marker);
            }
            markers.push("truncated");
            format_output_line(prefix, Some(&markers.join(",")), "")
        }
    }
}

fn line_ending_marker(ending: Option<LineEndingKind>) -> Option<&'static str> {
    match ending {
        Some(LineEndingKind::Lf) => None,
        Some(LineEndingKind::Crlf) => Some("crlf"),
        Some(LineEndingKind::Cr) => Some("cr"),
        None => Some("no_nl"),
    }
}

fn format_output_line(prefix: &str, marker: Option<&str>, content: &str) -> String {
    match marker {
        Some(marker) => format!("{prefix}({marker}) {content}"),
        None => format!("{prefix} {content}"),
    }
}

fn formatted_output_line_len(stream: OutputStream, content: &OutputContent) -> usize {
    if let OutputContent::Truncated {
        original_text_bytes,
        ..
    } = content
    {
        return stream.prefix().len() + 1 + original_text_bytes;
    }
    render_output_line(&OutputLine {
        stream,
        content: content.clone(),
    })
    .len()
}

#[cfg(not(unix))]
#[derive(Default)]
struct PipeOutput {
    lines: Vec<OutputLine>,
    had_invalid_utf8: bool,
}

#[cfg(not(unix))]
fn read_pipe(pipe: Option<impl std::io::Read>, stream: OutputStream) -> PipeOutput {
    let Some(mut pipe) = pipe else {
        return PipeOutput::default();
    };
    let mut decoder = StreamDecoder::default();
    let mut lines = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                lines.extend(decoder.push_bytes(&buf[..n]));
            }
        }
    }
    lines.extend(decoder.finish());
    PipeOutput {
        lines: lines
            .into_iter()
            .map(|content| OutputLine { stream, content })
            .collect(),
        had_invalid_utf8: decoder.had_invalid_utf8,
    }
}

fn command_display_args(command: &str) -> String {
    shorten_command_line(command.lines().next().unwrap_or_default())
}

fn shorten_command_line(line: &str) -> String {
    const EDGE_CHARS: usize = 20;
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= EDGE_CHARS * 2 {
        return line.to_owned();
    }

    let head: String = chars.iter().take(EDGE_CHARS).copied().collect();
    let tail: String = chars
        .iter()
        .skip(chars.len() - EDGE_CHARS)
        .copied()
        .collect();
    format!("{head}┄{tail}")
}

fn command_display_payload(command: &str) -> Option<ToolUsePayload> {
    if command.lines().count() < 2 {
        return None;
    }
    Some(ToolUsePayload::Text {
        text: command.to_owned(),
    })
}

pub(crate) struct CommandDetails {
    pub(crate) status: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) duration_seconds: Option<u64>,
    pub(crate) termination_reason: &'static str,
    pub(crate) total_lines: Option<usize>,
    pub(crate) total_bytes: Option<usize>,
    pub(crate) output: String,
    pub(crate) truncated: bool,
    pub(crate) valid_utf8: bool,
}

pub(crate) fn command_details_value(details: CommandDetails) -> CborValue {
    let CommandDetails {
        status,
        signal,
        timed_out,
        duration_seconds,
        termination_reason,
        total_lines,
        total_bytes,
        output,
        truncated,
        valid_utf8,
    } = details;
    let mut entries = vec![(
        CborValue::Text("output".to_owned()),
        CborValue::Text(output),
    )];
    if !valid_utf8 {
        entries.push((
            CborValue::Text("valid_utf8".to_owned()),
            CborValue::Bool(false),
        ));
    }
    if timed_out {
        entries.push((
            CborValue::Text("timed_out".to_owned()),
            CborValue::Bool(true),
        ));
    }
    if timed_out || signal.is_some() || status != Some(0) || termination_reason != "exit" {
        entries.push((
            CborValue::Text("termination_reason".to_owned()),
            CborValue::Text(termination_reason.to_owned()),
        ));
    }
    if truncated {
        entries.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
        if let Some(total_lines) = total_lines {
            entries.push((
                CborValue::Text("total_lines".to_owned()),
                CborValue::Integer((total_lines as i64).into()),
            ));
        }
        if let Some(total_bytes) = total_bytes {
            entries.push((
                CborValue::Text("total_bytes".to_owned()),
                CborValue::Integer((total_bytes as i64).into()),
            ));
        }
    }
    if let Some(status) = status {
        entries.push((
            CborValue::Text("status".to_owned()),
            CborValue::Integer(status.into()),
        ));
    }
    if let Some(signal) = signal {
        entries.push((
            CborValue::Text("signal".to_owned()),
            CborValue::Integer(signal.into()),
        ));
    }
    if let Some(duration_seconds) = duration_seconds {
        entries.push((
            CborValue::Text("duration_seconds".to_owned()),
            CborValue::Integer((duration_seconds as i64).into()),
        ));
    }
    CborValue::Map(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_args(command: &str, timeout: i64) -> CborValue {
        CborValue::Map(vec![
            (
                CborValue::Text("command".to_owned()),
                CborValue::Text(command.to_owned()),
            ),
            (
                CborValue::Text("timeout".to_owned()),
                CborValue::Integer(timeout.into()),
            ),
        ])
    }

    fn output_text(result: &CborValue) -> &str {
        let CborValue::Map(entries) = result else {
            panic!("expected result map");
        };
        entries
            .iter()
            .find_map(|(key, value)| match (key, value) {
                (CborValue::Text(key), CborValue::Text(value)) if key == "output" => {
                    Some(value.as_str())
                }
                _ => None,
            })
            .expect("output field")
    }

    /// Protects user shell clipping feedback for a single huge no-newline
    /// stdout stream. The final tail truncation must not drop the explicit
    /// marker that tells session history the captured context is incomplete.
    #[test]
    fn clipped_user_shell_output_marker_survives_tail_truncation() {
        let mut output = "x".repeat(MAX_OUTPUT_BYTES);

        append_guaranteed_output_truncated_marker(&mut output);
        let truncated = crate::truncate::truncate_tail(&output);

        assert!(truncated.content.ends_with(USER_OUTPUT_TRUNCATED_MARKER));
        assert!(truncated.content.len() <= MAX_OUTPUT_BYTES);
    }

    /// Ensures user shell progress streaming is independently bounded while
    /// output capture can keep draining the child pipes for final truncation.
    #[test]
    fn user_shell_progress_stream_is_bounded() {
        let mut capture = UserStreamCapture::default();
        let first = capture
            .progress_chunk(&"x".repeat(MAX_OUTPUT_BYTES - 1))
            .expect("first progress chunk");
        let marker = capture.progress_chunk("abcdef").expect("truncation marker");

        assert_eq!(first.len(), MAX_OUTPUT_BYTES - 1);
        assert_eq!(marker, USER_OUTPUT_TRUNCATED_MARKER);
        assert!(capture.progress_chunk("more").is_none());
    }

    fn record_cancelled_shell(
        cassette_dir: &std::path::Path,
        call_id: &str,
        timeout: i64,
    ) -> CborValue {
        let args = shell_args("sleep 10", timeout);
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let cassette_path = cassette_dir.to_owned();
        let args_for_thread = args.clone();
        let call_id = call_id.to_owned();
        let handle = std::thread::spawn(move || {
            let mut world = ShellWorld::for_tool(
                "shell",
                &call_id,
                &args_for_thread,
                Some(tau_vcr::VcrConfig::new(
                    tau_vcr::VcrMode::RecordIfMissing,
                    &cassette_path,
                )),
            )?;
            let outcome = run_command_cancellable(
                &call_id,
                &args_for_thread,
                &ShellConfig::default(),
                ShellCommandMode::READ_WRITE_HIDDEN,
                false,
                Some(cancel_rx),
                &mut world,
            );
            world.finish()?;
            outcome
        });
        std::thread::sleep(std::time::Duration::from_millis(25));
        cancel_tx.send(()).expect("send cancel");
        let outcome = handle
            .join()
            .expect("join recording")
            .expect("record shell");
        assert!(matches!(outcome, CommandOutcome::Cancelled));
        args
    }

    #[test]
    fn shell_vcr_replays_finished_result_without_running_command() {
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        let data_dir = tempfile::TempDir::new().expect("data dir");
        let file = data_dir.path().join("value.txt");
        std::fs::write(&file, "recorded-output").expect("write recorded value");
        let args = shell_args(&format!("cat {}", file.display()), 1);
        let mut world = ShellWorld::for_tool(
            "shell",
            "call_shell",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::RecordIfMissing,
                cassette_dir.path(),
            )),
        )
        .expect("record world");
        let recorded = run_command_cancellable(
            "call_shell",
            &args,
            &ShellConfig::default(),
            ShellCommandMode::READ_WRITE_HIDDEN,
            false,
            None,
            &mut world,
        )
        .expect("record shell");
        world.finish().expect("finish recording");
        assert!(matches!(recorded, CommandOutcome::Finished(_)));
        let cassette = std::fs::read_to_string(cassette_dir.path().join("call_shell.yaml"))
            .expect("read cassette");
        assert!(cassette.contains("op: shell"));
        std::fs::write(&file, "live-output").expect("write live value");

        let mut world = ShellWorld::for_tool(
            "shell",
            "call_shell",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::ReplayOnly,
                cassette_dir.path(),
            )),
        )
        .expect("replay world");
        let outcome = run_command_cancellable(
            "call_shell",
            &args,
            &ShellConfig::default(),
            ShellCommandMode::READ_WRITE_HIDDEN,
            false,
            None,
            &mut world,
        )
        .expect("replay shell");
        world.finish().expect("finish replay");

        let CommandOutcome::Finished(output) = outcome else {
            panic!("expected finished outcome");
        };
        assert_eq!(output_text(&output.result), "out(no_nl) recorded-output");
    }

    #[test]
    fn shell_vcr_finished_replay_sleeps_at_scaled_recorded_duration() {
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        let args = shell_args("printf live-output", 1);
        let mut world = ShellWorld::for_tool(
            "shell",
            "call_slow_finished_shell",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::RecordIfMissing,
                cassette_dir.path(),
            )),
        )
        .expect("record world");
        world.record_shell_outcome(WorldShellOutcome::Finished {
            result: CborValue::Map(vec![(
                CborValue::Text("output".to_owned()),
                CborValue::Text("out(no_nl) recorded-output".to_owned()),
            )]),
            display: Box::new(ok_display("recorded")),
            elapsed_ms: 5_000,
        });
        world.finish().expect("finish recording");

        let mut world = ShellWorld::for_tool(
            "shell",
            "call_slow_finished_shell",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::ReplayOnly,
                cassette_dir.path(),
            )),
        )
        .expect("replay world");
        let started = std::time::Instant::now();
        let outcome = run_command_cancellable(
            "call_slow_finished_shell",
            &args,
            &ShellConfig::default(),
            ShellCommandMode::READ_WRITE_HIDDEN,
            false,
            None,
            &mut world,
        )
        .expect("replay shell");
        let elapsed = started.elapsed();
        world.finish().expect("finish replay");

        assert!(
            elapsed >= std::time::Duration::from_millis(40),
            "replay should preserve scaled shell timing, elapsed={elapsed:?}"
        );
        let CommandOutcome::Finished(output) = outcome else {
            panic!("expected finished outcome");
        };
        assert_eq!(output_text(&output.result), "out(no_nl) recorded-output");
    }

    #[test]
    fn shell_vcr_cancelled_replay_requires_cancel_request() {
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        let args = record_cancelled_shell(cassette_dir.path(), "call_cancelled_shell", 1);
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let cassette_path = cassette_dir.path().to_owned();
        let args_for_thread = args.clone();
        let handle = std::thread::spawn(move || {
            let mut world = ShellWorld::for_tool(
                "shell",
                "call_cancelled_shell",
                &args_for_thread,
                Some(tau_vcr::VcrConfig::new(
                    tau_vcr::VcrMode::ReplayOnly,
                    &cassette_path,
                )),
            )?;
            let outcome = run_command_cancellable(
                "call_cancelled_shell",
                &args_for_thread,
                &ShellConfig::default(),
                ShellCommandMode::READ_WRITE_HIDDEN,
                false,
                Some(cancel_rx),
                &mut world,
            );
            world.finish()?;
            outcome
        });
        std::thread::sleep(std::time::Duration::from_millis(25));
        cancel_tx.send(()).expect("send cancel");

        let outcome = handle.join().expect("join replay").expect("replay shell");
        assert!(matches!(outcome, CommandOutcome::Cancelled));
    }

    #[test]
    fn shell_vcr_records_cancelled_outcome() {
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        record_cancelled_shell(cassette_dir.path(), "call_record_cancelled_shell", 5);
        let cassette =
            std::fs::read_to_string(cassette_dir.path().join("call_record_cancelled_shell.yaml"))
                .expect("read cassette");
        assert!(cassette.contains("op: shell"));
        assert!(cassette.contains("kind: cancelled"));
    }

    #[test]
    fn shell_vcr_cancelled_replay_errors_without_cancel_request() {
        let cassette_dir = tempfile::TempDir::new().expect("cassette dir");
        let args = record_cancelled_shell(cassette_dir.path(), "call_cancelled_shell", 1);
        let (_cancel_tx, cancel_rx) = mpsc::channel();
        let mut world = ShellWorld::for_tool(
            "shell",
            "call_cancelled_shell",
            &args,
            Some(tau_vcr::VcrConfig::new(
                tau_vcr::VcrMode::ReplayOnly,
                cassette_dir.path(),
            )),
        )
        .expect("replay world");

        let error = run_command_cancellable(
            "call_cancelled_shell",
            &args,
            &ShellConfig::default(),
            ShellCommandMode::READ_WRITE_HIDDEN,
            false,
            Some(cancel_rx),
            &mut world,
        )
        .expect_err("missing cancel should fail");

        assert!(error.message.contains("expected shell cancellation"));
    }
    /// Ensures a command that emits one huge line without a newline is captured
    /// as a truncated marker instead of retaining the whole line in memory.
    #[test]
    fn captured_output_bounds_no_newline_lines() {
        let mut output = CapturedOutput::default();
        output.push_bytes(
            OutputStream::Stdout,
            &vec![b'x'; MAX_CAPTURED_LINE_BYTES + 128],
        );
        output.finish();

        let truncated = output.truncate();
        assert_eq!(truncated.content, "out(no_nl,truncated) ");
        assert!(truncated.was_truncated);
        assert!(MAX_OUTPUT_BYTES < truncated.total_bytes);
    }
}

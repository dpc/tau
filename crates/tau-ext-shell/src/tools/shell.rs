//! `shell` tool and user-initiated `!`/`!!` command dispatch.

use std::{io as path_std_io, os as path_std_os, sync as path_std_sync, time as path_std_time};

use crate::tools as path_crate_tools;

#[cfg(test)]
mod tests;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use tau_proto::{
    CborValue, Event, HarnessInputMessage, ToolUsePayload, ToolUseState, ToolUseStatus,
};
use tracing::{debug, trace};

use crate::Output;
use crate::argument::{argument_text, optional_argument_int_strict, optional_argument_text};
use crate::config::ShellConfig;
use crate::display::{ToolFailure, ToolOutput, ok_display, text_stats};
use crate::shell_output_spool::{MAX_SAVED_OUTPUT_BYTES, SavedArtifact};
use crate::shell_process::{ShellProcess, ShellStderr, ShellStdout};
use crate::tools::world::{ShellWorld, WorldShellOutcome};
use crate::truncate::{
    MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, truncate_line_oriented_lines_with_byte_limit,
};

pub(crate) const DEFAULT_TIMEOUT: path_std_time::Duration = path_std_time::Duration::from_secs(300);
pub(crate) const SLOW_COMMAND_EXEC_TIME_THRESHOLD_SECS: u64 = 5;
const VCR_REPLAY_SPEEDUP: u64 = 100;
const MAX_CAPTURED_LINE_BYTES: usize = MAX_SAVED_OUTPUT_BYTES - 128;
const MAX_USER_STDERR_CHUNKS: usize = 4096;
pub(crate) const MAX_MODEL_SHELL_OUTPUT_BYTES: usize = 15 * 1024;
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
    let (args, payload) = command_display(&command);
    ToolUseState {
        args,
        mode: command_mode.display_label().unwrap_or_default().to_owned(),
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        payload,
        ..Default::default()
    }
}

/// Build a lock-wait display without copying an unbounded command.
pub(crate) fn bounded_lock_wait_display(
    arguments: &CborValue,
    command_mode: ShellCommandMode,
    max_bytes: usize,
) -> ToolUseState {
    let command = borrowed_text_field(arguments, "command").unwrap_or_default();
    let first_line = command.lines().next().unwrap_or_default();
    const EDGE_CHARS: usize = 20;
    let head = first_line.chars().take(EDGE_CHARS).collect::<String>();
    let tail = first_line
        .chars()
        .rev()
        .take(EDGE_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let shortened = first_line.chars().take(EDGE_CHARS * 2 + 1).count() > EDGE_CHARS * 2;
    let args = if shortened {
        format!("{head}…{tail}")
    } else {
        first_line.to_owned()
    };
    let payload = (command.contains('\n') || shortened).then(|| ToolUsePayload::Text {
        text: bounded_utf8_prefix(command, max_bytes),
    });
    ToolUseState {
        args,
        mode: command_mode.display_label().unwrap_or_default().to_owned(),
        status: ToolUseStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        payload,
        ..Default::default()
    }
}

fn borrowed_text_field<'a>(arguments: &'a CborValue, field: &str) -> Option<&'a str> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    entries.iter().find_map(|(key, value)| match (key, value) {
        (CborValue::Text(key), CborValue::Text(value)) if key == field => Some(value.as_str()),
        _ => None,
    })
}

fn bounded_utf8_prefix(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes.saturating_sub(3).min(text.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &text[..end])
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

/// Generic-shell test adapter for the production cancellable execution path.
#[cfg(test)]
pub(crate) fn run_command_cancellable(
    call_id: &str,
    arguments: &CborValue,
    shell_config: &ShellConfig,
    command_mode: ShellCommandMode,
    enforce_ro_bind: bool,
    cancel_rx: Option<mpsc::Receiver<()>>,
    world: &mut ShellWorld,
) -> Result<CommandOutcome, ToolFailure> {
    run_command_cancellable_for_tool(
        ShellInvocation {
            surface: path_crate_tools::ShellSurface::Generic,
            call_id,
            arguments,
            authorized_cwd: None,
        },
        shell_config,
        command_mode,
        enforce_ro_bind,
        cancel_rx,
        world,
    )
}

/// Identity and arguments for one shell tool invocation.
pub(crate) struct ShellInvocation<'a> {
    /// Provider-facing shell surface selecting its argument dialect.
    pub(crate) surface: crate::tools::ShellSurface,
    /// Stable tool call id used by recording and replay.
    pub(crate) call_id: &'a str,
    /// Decoded function arguments for this invocation.
    pub(crate) arguments: &'a CborValue,
    /// Canonical operational cwd retained from pre-VCR authorization.
    pub(crate) authorized_cwd: Option<&'a Path>,
}

/// Execute one shell tool call using that surface's directory argument.
pub(crate) fn run_command_cancellable_for_tool(
    invocation: ShellInvocation<'_>,
    shell_config: &ShellConfig,
    command_mode: ShellCommandMode,
    enforce_ro_bind: bool,
    cancel_rx: Option<mpsc::Receiver<()>>,
    world: &mut ShellWorld,
) -> Result<CommandOutcome, ToolFailure> {
    let ShellInvocation {
        surface,
        call_id,
        arguments,
        authorized_cwd,
    } = invocation;
    validate_surface_arguments(surface, arguments)?;
    if let Some(outcome) = world.replay_shell_outcome()? {
        return replay_shell_outcome(call_id, outcome, arguments, cancel_rx);
    }

    let started = path_std_time::Instant::now();
    let outcome = run_command_live_for_surface_with_authorized_cwd(
        surface,
        arguments,
        shell_config,
        command_mode,
        enforce_ro_bind,
        cancel_rx,
        authorized_cwd,
    )?;
    let elapsed_ms = elapsed_millis(started.elapsed());
    let recorded = match &outcome {
        CommandOutcome::Finished(output) => WorldShellOutcome::Finished {
            result: output.result.clone(),
            display: Box::new(output.display.clone()),
            elapsed_ms,
            saved_output: None,
        },
        CommandOutcome::Cancelled => WorldShellOutcome::Cancelled,
    };
    world.record_shell_outcome(recorded);
    Ok(outcome)
}

/// Generic-shell test adapter for direct live process execution.
#[cfg(test)]
pub(crate) fn run_command_live(
    arguments: &CborValue,
    shell_config: &ShellConfig,
    command_mode: ShellCommandMode,
    enforce_ro_bind: bool,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<CommandOutcome, ToolFailure> {
    run_command_live_for_surface(
        path_crate_tools::ShellSurface::Generic,
        arguments,
        shell_config,
        command_mode,
        enforce_ro_bind,
        cancel_rx,
    )
}

/// Run one live shell tool call using that surface's directory argument.
#[cfg(test)]
pub(crate) fn run_command_live_for_surface(
    surface: crate::tools::ShellSurface,
    arguments: &CborValue,
    shell_config: &ShellConfig,
    command_mode: ShellCommandMode,
    enforce_ro_bind: bool,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<CommandOutcome, ToolFailure> {
    let authorized_cwd = prepare_tool_invocation(surface, arguments, shell_config)?;
    run_command_live_for_surface_with_authorized_cwd(
        surface,
        arguments,
        shell_config,
        command_mode,
        enforce_ro_bind,
        cancel_rx,
        authorized_cwd.as_deref(),
    )
}

/// Run one live shell tool call after any pre-replay authorization.
fn run_command_live_for_surface_with_authorized_cwd(
    surface: crate::tools::ShellSurface,
    arguments: &CborValue,
    shell_config: &ShellConfig,
    command_mode: ShellCommandMode,
    enforce_ro_bind: bool,
    cancel_rx: Option<mpsc::Receiver<()>>,
    authorized_cwd: Option<&std::path::Path>,
) -> Result<CommandOutcome, ToolFailure> {
    let command = argument_text(arguments, "command").map_err(ToolFailure::from)?;
    validate_surface_arguments(surface, arguments)?;
    let cwd = optional_argument_text(arguments, surface.directory_argument())
        .map_err(ToolFailure::from)?;
    let cwd = authorized_cwd
        .map(Path::to_path_buf)
        .or_else(|| cwd.map(PathBuf::from));
    let display_mode = command_mode.display_label().unwrap_or_default();
    let (display_args, display_payload) = command_display(&command);
    let timeout = parse_timeout(arguments).map_err(|message| {
        ToolFailure::from(message)
            .with_args(display_args.clone())
            .with_mode(display_mode)
            .with_payload(display_payload.clone())
    })?;

    debug!(
        command = %command,
        cwd = ?cwd,
        timeout_secs = timeout.as_secs(),
        "starting shell command"
    );
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
                    saved_output: None,
                }))
        })?;

    let child_id = child.child.id();
    debug!(child_id, "shell command spawned");
    let started = path_std_time::Instant::now();
    let wait = wait_with_timeout(child, timeout, cancel_rx);
    let elapsed = started.elapsed();
    let duration_seconds =
        if path_std_time::Duration::from_secs(SLOW_COMMAND_EXEC_TIME_THRESHOLD_SECS) < elapsed {
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

    let saved_output = output_trunc.was_truncated.then(|| {
        match crate::shell_output_spool::save(
            &wait.output.saved_output,
            wait.output.saved_output_incomplete,
        ) {
            Ok(saved) => SavedArtifact::Available(saved),
            Err(_) => SavedArtifact::Unavailable,
        }
    });
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
        saved_output,
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
        provider_content: Vec::new(),
        display,
    })))
}

/// Validate and authorize a shell invocation before any VCR access or spawn.
pub(crate) fn prepare_tool_invocation(
    surface: crate::tools::ShellSurface,
    arguments: &CborValue,
    shell_config: &ShellConfig,
) -> Result<Option<PathBuf>, ToolFailure> {
    let command = argument_text(arguments, "command").map_err(ToolFailure::from)?;
    validate_surface_arguments(surface, arguments)?;
    parse_timeout(arguments).map_err(ToolFailure::from)?;
    let cwd = optional_argument_text(arguments, surface.directory_argument())
        .map_err(ToolFailure::from)?
        .map(PathBuf::from)
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|error| {
            ToolFailure::from(format!("failed to resolve shell command workdir: {error}"))
        })?;
    shell_config
        .authorize(&command, &cwd)
        .map_err(ToolFailure::from)
}

fn validate_surface_arguments(
    surface: crate::tools::ShellSurface,
    arguments: &CborValue,
) -> Result<(), ToolFailure> {
    if surface == path_crate_tools::ShellSurface::ChatGpt
        && optional_argument_text(arguments, "cwd")
            .map_err(ToolFailure::from)?
            .is_some()
    {
        return Err(ToolFailure::new(
            "argument `cwd` is not supported by `shell_command`; use call-local `workdir`"
                .to_owned(),
        ));
    }
    optional_argument_text(arguments, surface.directory_argument()).map_err(ToolFailure::from)?;
    Ok(())
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
            saved_output: _,
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
                provider_content: Vec::new(),
                display: *display,
            })))
        }
        WorldShellOutcome::Cancelled => {
            let timeout = parse_timeout(arguments).map_err(ToolFailure::from)?;
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
    std::thread::sleep(path_std_time::Duration::from_millis(
        elapsed_ms.div_ceil(VCR_REPLAY_SPEEDUP),
    ));
}

fn parse_timeout(arguments: &CborValue) -> Result<path_std_time::Duration, String> {
    let Some(timeout) = optional_argument_int_strict(arguments, "timeout")? else {
        return Ok(DEFAULT_TIMEOUT);
    };
    if timeout < 0 {
        return Err("argument `timeout` must be non-negative".to_owned());
    }
    Ok(path_std_time::Duration::from_secs(timeout as u64))
}

/// Run a user-initiated `!`/`!!` shell command, streaming stdout and
/// stderr back as `ShellCommandProgress` chunks while they arrive and
/// emitting `ShellCommandFinished` with bounded native output and saved-output
/// recovery metadata when the child exits.
pub(crate) fn dispatch_user_shell_command(
    cmd: tau_proto::UiShellCommand,
    shell_config: ShellConfig,
    tx: &Output,
    cancel_rx: mpsc::Receiver<()>,
    cwd: PathBuf,
) {
    crate::shell_output_spool::note_call();
    let cwd = match shell_config.authorize(&cmd.command, &cwd) {
        Ok(Some(canonical)) => canonical,
        Ok(None) => cwd,
        Err(error) => {
            send_user_shell_finished(cmd, error, None, false, tx);
            return;
        }
    };
    let child = match shell_config.spawn_isolated(&cmd.command, Some(&cwd), false, false) {
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
    dispatch_user_shell_command_unix(cmd, child, shell_config.user_command_timeout, tx, cancel_rx);
    #[cfg(not(unix))]
    dispatch_user_shell_command_blocking(
        cmd,
        child,
        shell_config.user_command_timeout,
        tx,
        cancel_rx,
    );
}

fn send_user_shell_finished(
    cmd: tau_proto::UiShellCommand,
    output: String,
    exit_code: Option<i32>,
    cancelled: bool,
    tx: &Output,
) {
    let _ = tx.send_checked(HarnessInputMessage::emit(
        Event::ShellCommandFinishedReported(tau_proto::ShellCommandFinished {
            command_id: cmd.command_id,
            session_id: cmd.session_id,
            command: cmd.command,
            include_in_context: cmd.include_in_context,
            target_agent_id: cmd.target_agent_id,
            output,
            exit_code,
            cancelled,
        }),
    ));
}

/// Bounded capture state for one user-shell output stream.
#[derive(Default)]
struct UserStreamCapture {
    /// Visible prefix retained for final context.
    captured: String,
    /// Whether visible capture omitted later bytes.
    clipped: bool,
    /// Complete decoded byte count observed on this stream.
    total_bytes: usize,
    /// Complete newline count observed on this stream.
    newline_count: usize,
    /// Whether the latest observed byte was a newline.
    ends_with_newline: bool,
}

impl UserStreamCapture {
    fn push_chunk(&mut self, chunk: &str) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());
        self.newline_count = self
            .newline_count
            .saturating_add(chunk.bytes().filter(|byte| *byte == b'\n').count());
        if let Some(last) = chunk.as_bytes().last() {
            self.ends_with_newline = *last == b'\n';
        }
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
}

/// One ordered, aggregate-bounded retained user-shell artifact.
///
/// `bytes` is one fixed 16 MiB arena. Valid UTF-8 stdout occupies
/// `0..stdout_len`. Valid UTF-8 stderr chunks occupy disjoint ranges allocated
/// backward from `stderr_cursor`; `stderr_chunks` records those ranges in
/// arrival order. The native rendering is stdout, an optional newline, the
/// stderr label, then chunks in descriptor order. All lengths are character
/// boundaries and stdout growth evicts whole latest stderr chunks, so capture
/// remains a prefix without moving retained bytes. Descriptor count is at most
/// `MAX_USER_STDERR_CHUNKS`; range lengths sum to `stderr_bytes`, every range
/// begins at or above `stderr_cursor`, and no range overlaps stdout.
/// `stdout_len <= stderr_cursor <= bytes.len()`; the cursor equals the lowest
/// range start when stderr is nonempty and `bytes.len()` otherwise.
struct UserSavedCapture {
    /// Fixed-size arena containing stdout and stderr regions.
    bytes: Vec<u8>,
    /// End of the stdout prefix at the arena front.
    stdout_len: usize,
    /// Lowest arena offset occupied by reverse-allocated stderr.
    stderr_cursor: usize,
    /// Stderr chunk ranges in native arrival order.
    stderr_chunks: Vec<std::ops::Range<usize>>,
    /// Total retained stderr content bytes.
    stderr_bytes: usize,
    /// Whether stdout has omitted a byte and must stop accepting suffixes.
    stdout_incomplete: bool,
    /// Whether stderr has omitted a byte and must stop accepting suffixes.
    stderr_incomplete: bool,
    /// Whether either native section omitted bytes.
    incomplete: bool,
}

impl Default for UserSavedCapture {
    fn default() -> Self {
        Self {
            bytes: vec![0; MAX_SAVED_OUTPUT_BYTES],
            stdout_len: 0,
            stderr_cursor: MAX_SAVED_OUTPUT_BYTES,
            stderr_chunks: Vec::with_capacity(MAX_USER_STDERR_CHUNKS),
            stderr_bytes: 0,
            stdout_incomplete: false,
            stderr_incomplete: false,
            incomplete: false,
        }
    }
}

impl UserSavedCapture {
    const STDERR_LABEL: &'static str = "[stderr]\n";

    fn framing_len(&self) -> usize {
        if self.stderr_bytes == 0 {
            0
        } else {
            Self::STDERR_LABEL.len() + usize::from(self.stdout_len != 0)
        }
    }

    fn push(&mut self, stream: tau_proto::ShellStream, chunk: &str) {
        match stream {
            tau_proto::ShellStream::Stdout => {
                if self.stdout_incomplete {
                    return;
                }
                let mut accepted = chunk.len().min(
                    MAX_SAVED_OUTPUT_BYTES
                        .saturating_sub(self.stdout_len)
                        .saturating_sub(self.framing_len()),
                );
                while !chunk.is_char_boundary(accepted) {
                    accepted -= 1;
                }
                while self.stderr_cursor
                    < self
                        .stdout_len
                        .saturating_add(accepted)
                        .saturating_add(self.framing_len())
                {
                    let Some(range) = self.stderr_chunks.pop() else {
                        break;
                    };
                    self.stderr_cursor = range.end;
                    self.stderr_bytes -= range.len();
                    self.stderr_incomplete = true;
                    self.incomplete = true;
                }
                let available = self
                    .stderr_cursor
                    .saturating_sub(self.stdout_len)
                    .saturating_sub(self.framing_len());
                accepted = accepted.min(available);
                while !chunk.is_char_boundary(accepted) {
                    accepted -= 1;
                }
                if accepted < chunk.len() {
                    self.stderr_chunks.clear();
                    self.stderr_cursor = MAX_SAVED_OUTPUT_BYTES;
                    self.stderr_bytes = 0;
                    self.stderr_incomplete = true;
                }
                self.incomplete |= accepted < chunk.len();
                self.stdout_incomplete |= accepted < chunk.len();
                let new_stdout_len = self.stdout_len + accepted;
                self.bytes[self.stdout_len..new_stdout_len]
                    .copy_from_slice(&chunk.as_bytes()[..accepted]);
                self.stdout_len = new_stdout_len;
            }
            tau_proto::ShellStream::Stderr => {
                if self.stderr_incomplete {
                    return;
                }
                if MAX_USER_STDERR_CHUNKS <= self.stderr_chunks.len() {
                    self.stderr_incomplete = true;
                    self.incomplete = true;
                    return;
                }
                let overhead = Self::STDERR_LABEL.len() + usize::from(0 < self.stdout_len);
                let remaining = self
                    .stderr_cursor
                    .saturating_sub(self.stdout_len)
                    .saturating_sub(overhead);
                let mut accepted = remaining.min(chunk.len());
                while !chunk.is_char_boundary(accepted) {
                    accepted -= 1;
                }
                if accepted != 0 {
                    self.stderr_cursor -= accepted;
                    self.bytes[self.stderr_cursor..self.stderr_cursor + accepted]
                        .copy_from_slice(&chunk.as_bytes()[..accepted]);
                    self.stderr_chunks
                        .push(self.stderr_cursor..self.stderr_cursor + accepted);
                    self.stderr_bytes += accepted;
                }
                self.stderr_incomplete |= accepted < chunk.len();
                self.incomplete |= accepted < chunk.len();
            }
        }
    }

    fn stdout(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.stdout_len])
            .expect("saved user stdout remains UTF-8")
    }

    fn stderr_parts(&self) -> impl Iterator<Item = &str> {
        self.stderr_chunks.iter().map(|range| {
            std::str::from_utf8(&self.bytes[range.clone()])
                .expect("saved user stderr remains UTF-8")
        })
    }

    fn rendering_parts(&self) -> Vec<&str> {
        let mut parts = Vec::with_capacity(self.stderr_chunks.len() + 3);
        parts.push(self.stdout());
        if self.stderr_bytes != 0 {
            if self.stdout_len != 0 {
                parts.push("\n");
            }
            parts.push(Self::STDERR_LABEL);
            parts.extend(self.stderr_parts());
        }
        parts
    }
}

/// Shared saved-output capture and live-progress state for a user shell.
#[derive(Default)]
struct UserCaptureState {
    /// Ordered retained artifact capture state.
    saved: UserSavedCapture,
    /// Live progress budget.
    progress: UserProgressBudget,
}

/// Shared live-progress budget across user-shell stdout and stderr.
#[derive(Default)]
struct UserProgressBudget {
    /// Bytes forwarded across both streams.
    bytes: usize,
    /// Whether the shared budget has emitted its truncation marker.
    clipped: bool,
}

impl UserProgressBudget {
    fn chunk(&mut self, chunk: &str) -> Option<String> {
        if self.clipped {
            return None;
        }
        let content_limit = MAX_OUTPUT_BYTES.saturating_sub(USER_OUTPUT_TRUNCATED_MARKER.len());
        if self.bytes < content_limit {
            let remaining = content_limit - self.bytes;
            let mut end = remaining.min(chunk.len());
            while !chunk.is_char_boundary(end) {
                end -= 1;
            }
            self.bytes += end;
            if end == chunk.len() {
                return Some(chunk.to_owned());
            }
            self.bytes = MAX_OUTPUT_BYTES;
            self.clipped = true;
            return Some(format!("{}{USER_OUTPUT_TRUNCATED_MARKER}", &chunk[..end]));
        }
        self.clipped = true;
        self.bytes = MAX_OUTPUT_BYTES;
        Some(USER_OUTPUT_TRUNCATED_MARKER.to_owned())
    }
}

fn merged_user_shell_output(
    stdout: UserStreamCapture,
    stderr: UserStreamCapture,
    saved: UserSavedCapture,
    status_note: Option<String>,
) -> String {
    let total_bytes = stdout
        .total_bytes
        .saturating_add(stderr.total_bytes)
        .saturating_add(usize::from(
            0 < stdout.total_bytes && 0 < stderr.total_bytes,
        ))
        .saturating_add(if 0 < stderr.total_bytes {
            "[stderr]\n".len()
        } else {
            0
        });
    let total_lines = stdout
        .newline_count
        .saturating_add(stderr.newline_count)
        .saturating_add(usize::from(
            0 < stdout.total_bytes && !stdout.ends_with_newline,
        ))
        .saturating_add(usize::from(
            0 < stderr.total_bytes && !stderr.ends_with_newline,
        ))
        .saturating_add(usize::from(0 < stderr.total_bytes))
        .saturating_add(usize::from(
            0 < stdout.total_bytes && stdout.ends_with_newline && 0 < stderr.total_bytes,
        ));
    let stream_clipped = stdout.clipped || stderr.clipped;
    let mut merged = stdout.captured;
    if !stderr.captured.is_empty() {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str("[stderr]\n");
        merged.push_str(&stderr.captured);
    }
    if stream_clipped {
        append_guaranteed_output_truncated_marker(&mut merged);
    }
    if let Some(note) = status_note {
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(&note);
    }
    let clipped = stream_clipped
        || MAX_OUTPUT_BYTES < merged.len()
        || MAX_OUTPUT_LINES < merged.lines().count();
    let mut footer = String::new();
    if clipped {
        footer.push_str("\n\n[tau-output-metadata]\ntruncated: true");
        footer.push_str(&format!(
            "\ntotal_lines: {total_lines}\ntotal_bytes: {total_bytes}\ntruncation_warning: fetching excessive output is inefficient; prefer narrower commands or filters"
        ));
        let parts = saved.rendering_parts();
        match crate::shell_output_spool::save_parts(&parts, saved.incomplete) {
            Ok(saved) => {
                let label = if saved.incomplete {
                    "saved_output_path"
                } else {
                    "full_output_path"
                };
                let mut artifact_fields = format!("\n{label}: {}", saved.path.display());
                if saved.incomplete {
                    artifact_fields.push_str(&format!(
                        "\nsaved_output_truncated: true\nsaved_output_bytes: {}",
                        saved.saved_bytes
                    ));
                }
                if footer.len().saturating_add(artifact_fields.len()) <= MAX_OUTPUT_BYTES {
                    footer.push_str(&artifact_fields);
                } else {
                    footer.push_str("\nsaved_output_unavailable: true");
                }
            }
            Err(_) => footer.push_str("\nsaved_output_unavailable: true"),
        }
    }
    let content_budget = MAX_OUTPUT_BYTES.saturating_sub(footer.len());
    let visible = crate::truncate::truncate_line_oriented_lines_with_byte_limit(
        merged.lines(),
        merged.lines().count(),
        merged.len(),
        content_budget,
    )
    .content;
    format!("{visible}{footer}")
}

#[cfg(unix)]
#[cfg(unix)]
const USER_SHELL_READ_CHUNK_BYTES: usize = 8192;
#[cfg(unix)]
const USER_SHELL_DRAIN_AFTER_DONE: std::time::Duration = path_std_time::Duration::from_millis(50);

#[cfg(unix)]
fn set_user_shell_nonblocking(fd: path_std_os::fd::RawFd) {
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
    capture_state: &mut UserCaptureState,
    tx: &Output,
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
                capture_state.saved.push(stream, &chunk);
                if let Some(chunk) = capture_state.progress.chunk(&chunk) {
                    let _ = tx.send(HarnessInputMessage::emit(
                        Event::ShellCommandProgressReported(tau_proto::ShellCommandProgress {
                            command_id: command_id.clone(),
                            stream,
                            chunk,
                            target_agent_id: target_agent_id.clone(),
                        }),
                    ));
                }
            }
            Err(error) if error.kind() == path_std_io::ErrorKind::WouldBlock => break,
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
enum UserShellEvent {
    Exited(std::process::ExitStatus),
    WaitFailed,
    Cancelled,
}

#[cfg(unix)]
fn apply_user_shell_event(
    event: UserShellEvent,
    status: &mut Option<std::process::ExitStatus>,
    wait_failed: &mut bool,
    cancelled: &mut bool,
) -> bool {
    match event {
        UserShellEvent::Exited(received) => {
            *status = Some(received);
            true
        }
        UserShellEvent::WaitFailed => {
            *wait_failed = true;
            true
        }
        UserShellEvent::Cancelled => {
            *cancelled = true;
            false
        }
    }
}

#[cfg(unix)]
fn collect_user_shell_status(
    event_rx: &mpsc::Receiver<UserShellEvent>,
    status: &mut Option<std::process::ExitStatus>,
    wait_failed: &mut bool,
    cancelled: &mut bool,
) -> bool {
    use std::sync::mpsc::TryRecvError;

    if status.is_some() || *wait_failed {
        return true;
    }
    match event_rx.try_recv() {
        Ok(event) => apply_user_shell_event(event, status, wait_failed, cancelled),
        Err(TryRecvError::Disconnected) => {
            *wait_failed = true;
            true
        }
        Err(TryRecvError::Empty) => false,
    }
}

#[cfg(unix)]
fn wait_for_user_shell_event_until(
    event_rx: &mpsc::Receiver<UserShellEvent>,
    deadline: std::time::Instant,
    status: &mut Option<std::process::ExitStatus>,
    wait_failed: &mut bool,
    cancelled: &mut bool,
) {
    let timeout = deadline.saturating_duration_since(path_std_time::Instant::now());
    match event_rx.recv_timeout(timeout) {
        Ok(event) => {
            let _ = apply_user_shell_event(event, status, wait_failed, cancelled);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            *wait_failed = true;
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {}
    }
}

#[cfg(unix)]
fn user_shell_poll_timeout_ms(deadline: std::time::Instant) -> i32 {
    let now = path_std_time::Instant::now();
    if deadline <= now {
        return 0;
    }
    let remaining = deadline - now;
    i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX)
}

#[cfg(unix)]
fn drain_user_shell_wake_fd(wake_read: &path_std_os::fd::OwnedFd) {
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
fn push_user_shell_poll_fd(poll_fds: &mut Vec<libc::pollfd>, fd: path_std_os::fd::RawFd) {
    poll_fds.push(libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    });
}

#[cfg(unix)]
fn user_shell_poll_fds(
    stdout_pipe: Option<&ShellStdout>,
    stderr_pipe: Option<&ShellStderr>,
    wake_read: Option<&path_std_os::fd::OwnedFd>,
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
    mut process: ShellProcess,
    timeout: path_std_time::Duration,
    tx: &Output,
    cancel_rx: mpsc::Receiver<()>,
) {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let pid = process.child.id();
    debug!(
        pid,
        timeout_ms = timeout.as_millis(),
        "waiting for user shell child"
    );

    let mut stdout_pipe = process.stdout.take();
    let mut stderr_pipe = process.stderr.take();
    #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
    let mut output_users = process.output_users.take();
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
    let cancel_wake_write = wake_write.as_ref().and_then(|wake_write| {
        #[allow(unsafe_code)]
        let fd = unsafe { libc::dup(wake_write.as_raw_fd()) };
        if 0 <= fd {
            #[allow(unsafe_code)]
            unsafe {
                Some(OwnedFd::from_raw_fd(fd))
            }
        } else {
            None
        }
    });
    let (event_tx, event_rx) = mpsc::channel::<UserShellEvent>();
    let cancel_event_tx = event_tx.clone();
    let cancelled_by_request = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&cancelled_by_request);
    let _cancel_waiter = std::thread::spawn(move || {
        if cancel_rx.recv().is_ok() {
            cancel_flag.store(true, Ordering::SeqCst);
            let _ = cancel_event_tx.send(UserShellEvent::Cancelled);
            if let Some(wake_write) = cancel_wake_write {
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
        }
    });

    let _waiter = std::thread::spawn(move || {
        let _wake_read_guard = waiter_wake_read;
        let status = process.child.wait();
        debug!(pid, status = ?status, "user shell child waiter finished");
        match status {
            Ok(status) => {
                let _ = event_tx.send(UserShellEvent::Exited(status));
            }
            Err(_) => {
                let _ = event_tx.send(UserShellEvent::WaitFailed);
            }
        }
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
    let mut capture_state = UserCaptureState::default();
    let mut status = None;
    let mut wait_failed = false;
    let mut timed_out = false;
    let mut cancelled = false;
    let deadline = path_std_time::Instant::now() + timeout;

    loop {
        read_available_user_shell(
            &mut stdout_pipe,
            tau_proto::ShellStream::Stdout,
            &cmd.command_id,
            &cmd.target_agent_id,
            &mut stdout,
            &mut capture_state,
            tx,
        );
        read_available_user_shell(
            &mut stderr_pipe,
            tau_proto::ShellStream::Stderr,
            &cmd.command_id,
            &cmd.target_agent_id,
            &mut stderr,
            &mut capture_state,
            tx,
        );
        if collect_user_shell_status(&event_rx, &mut status, &mut wait_failed, &mut cancelled) {
            break;
        }
        if cancelled_by_request.load(Ordering::SeqCst) {
            cancelled = true;
            kill_process_group_by_pid(pid);
            break;
        }
        let now = path_std_time::Instant::now();
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
            wait_for_user_shell_event_until(
                &event_rx,
                deadline,
                &mut status,
                &mut wait_failed,
                &mut cancelled,
            );
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
        if cancelled_by_request.load(Ordering::SeqCst) {
            cancelled = true;
            kill_process_group_by_pid(pid);
            break;
        }
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
    drop(output_users.take());
    let drain_deadline = path_std_time::Instant::now() + USER_SHELL_DRAIN_AFTER_DONE;
    loop {
        read_available_user_shell(
            &mut stdout_pipe,
            tau_proto::ShellStream::Stdout,
            &cmd.command_id,
            &cmd.target_agent_id,
            &mut stdout,
            &mut capture_state,
            tx,
        );
        read_available_user_shell(
            &mut stderr_pipe,
            tau_proto::ShellStream::Stderr,
            &cmd.command_id,
            &cmd.target_agent_id,
            &mut stderr,
            &mut capture_state,
            tx,
        );
        let _ = collect_user_shell_status(&event_rx, &mut status, &mut wait_failed, &mut cancelled);
        if (stdout_pipe.is_none() && stderr_pipe.is_none())
            || drain_deadline <= path_std_time::Instant::now()
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
        Some(format!(
            "command killed after {}s timeout",
            timeout.as_secs()
        ))
    } else if cancelled {
        Some("command cancelled".to_owned())
    } else if wait_failed {
        Some("wait failed".to_owned())
    } else {
        None
    };
    let output = merged_user_shell_output(stdout, stderr, capture_state.saved, status_note);
    send_user_shell_finished(cmd, output, exit_code, timed_out || cancelled, tx);
}

#[cfg(not(unix))]
fn dispatch_user_shell_command_blocking(
    cmd: tau_proto::UiShellCommand,
    mut process: ShellProcess,
    timeout: path_std_time::Duration,
    tx: &Output,
    cancel_rx: mpsc::Receiver<()>,
) {
    use std::io::Read;

    fn pump<R: Read + Send + 'static>(
        mut pipe: R,
        stream: tau_proto::ShellStream,
        command_id: tau_proto::ShellCommandId,
        target_agent_id: Option<tau_proto::AgentId>,
        tx: Output,
        capture: std::sync::Arc<std::sync::Mutex<UserStreamCapture>>,
        progress: std::sync::Arc<std::sync::Mutex<UserProgressBudget>>,
        saved_capture: std::sync::Arc<std::sync::Mutex<UserSavedCapture>>,
        stop: std::sync::Arc<path_std_sync::atomic::AtomicBool>,
        progress_gate: std::sync::Arc<std::sync::Mutex<()>>,
        done_tx: mpsc::Sender<()>,
    ) {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                        if stop.load(path_std_sync_atomic::Ordering::SeqCst) {
                            break;
                        }
                        {
                            let mut capture =
                                capture.lock().unwrap_or_else(|error| error.into_inner());
                            let mut saved_capture = saved_capture
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            capture.push_chunk(&chunk);
                            saved_capture.push(stream, &chunk);
                        }
                        let _progress_guard = progress_gate
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        let progress_chunk = progress
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .chunk(&chunk);
                        if let Some(chunk) = progress_chunk {
                            if stop.load(path_std_sync_atomic::Ordering::SeqCst) {
                                break;
                            }
                            let _ = tx.send(HarnessInputMessage::emit(
                                Event::ShellCommandProgressReported(
                                    tau_proto::ShellCommandProgress {
                                        command_id: command_id.clone(),
                                        stream,
                                        chunk,
                                        target_agent_id: target_agent_id.clone(),
                                    },
                                ),
                            ));
                        }
                    }
                }
            }
            let _ = done_tx.send(());
        });
    }

    let stdout_pipe = process.stdout.take();
    let stderr_pipe = process.stderr.take();
    let stdout = path_std_sync::Arc::new(path_std_sync::Mutex::new(UserStreamCapture::default()));
    let stderr = path_std_sync::Arc::new(path_std_sync::Mutex::new(UserStreamCapture::default()));
    let stop_pipe_readers = path_std_sync::Arc::new(path_std_sync_atomic::AtomicBool::new(false));
    let progress_gate = path_std_sync::Arc::new(path_std_sync::Mutex::new(()));
    let progress =
        path_std_sync::Arc::new(path_std_sync::Mutex::new(UserProgressBudget::default()));
    let saved_capture =
        path_std_sync::Arc::new(path_std_sync::Mutex::new(UserSavedCapture::default()));
    let (pipe_done_tx, pipe_done_rx) = mpsc::channel();
    if let Some(p) = stdout_pipe {
        pump(
            p,
            tau_proto::ShellStream::Stdout,
            cmd.command_id.clone(),
            cmd.target_agent_id.clone(),
            tx.clone(),
            path_std_sync::Arc::clone(&stdout),
            path_std_sync::Arc::clone(&progress),
            path_std_sync::Arc::clone(&saved_capture),
            path_std_sync::Arc::clone(&stop_pipe_readers),
            path_std_sync::Arc::clone(&progress_gate),
            pipe_done_tx.clone(),
        );
    } else {
        let _ = pipe_done_tx.send(());
    }
    if let Some(p) = stderr_pipe {
        pump(
            p,
            tau_proto::ShellStream::Stderr,
            cmd.command_id.clone(),
            cmd.target_agent_id.clone(),
            tx.clone(),
            path_std_sync::Arc::clone(&stderr),
            path_std_sync::Arc::clone(&progress),
            path_std_sync::Arc::clone(&saved_capture),
            path_std_sync::Arc::clone(&stop_pipe_readers),
            path_std_sync::Arc::clone(&progress_gate),
            pipe_done_tx,
        );
    } else {
        let _ = pipe_done_tx.send(());
    }

    let child_wait = NonUnixChildWait::start(&process.child, timeout, Some(cancel_rx));
    let pid = process.child.id();
    debug!(
        pid,
        timeout_ms = timeout.as_millis(),
        "waiting for user shell child"
    );
    let child_event = child_wait.recv();
    let (status, status_note, cancelled) = match child_event {
        NonUnixChildEvent::Exited => (process.child.wait().ok(), None, false),
        NonUnixChildEvent::Cancelled => match process.child.try_wait() {
            Ok(Some(status)) => (Some(status), None, false),
            _ => {
                let _ = process.child.kill();
                (
                    process.child.wait().ok(),
                    Some("command cancelled".to_owned()),
                    true,
                )
            }
        },
        NonUnixChildEvent::TimedOut => match process.child.try_wait() {
            Ok(Some(status)) => (Some(status), None, false),
            _ => {
                let _ = process.child.kill();
                (
                    process.child.wait().ok(),
                    Some(format!(
                        "command killed after {}s timeout",
                        timeout.as_secs()
                    )),
                    true,
                )
            }
        },
        NonUnixChildEvent::WaitFailed => {
            let _ = process.child.kill();
            (
                process.child.wait().ok(),
                Some("wait failed".to_owned()),
                false,
            )
        }
    };
    child_wait.join_exit_and_timeout_watchers();
    drain_nonunix_pipe_captures(&pipe_done_rx, NON_UNIX_PIPE_CAPTURE_COUNT);
    {
        let _progress_guard = progress_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        stop_pipe_readers.store(true, path_std_sync_atomic::Ordering::SeqCst);
    }
    let exit_code = status.and_then(|status| status.code());

    let stdout = std::mem::take(&mut *stdout.lock().unwrap_or_else(|error| error.into_inner()));
    let stderr = std::mem::take(&mut *stderr.lock().unwrap_or_else(|error| error.into_inner()));
    let saved = std::mem::take(
        &mut *saved_capture
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    );
    let output = merged_user_shell_output(stdout, stderr, saved, status_note);
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

const SHELL_WAIT_READ_CHUNK_BYTES: usize = 8192;
#[cfg(unix)]
const SHELL_WAIT_DRAIN_AFTER_DONE: std::time::Duration = path_std_time::Duration::from_millis(50);
#[cfg(not(unix))]
const NON_UNIX_PIPE_DRAIN_AFTER_DONE: std::time::Duration =
    path_std_time::Duration::from_millis(50);
#[cfg(not(unix))]
const NON_UNIX_PIPE_CAPTURE_COUNT: usize = 2;

#[cfg(unix)]
fn shell_wait_set_nonblocking(fd: path_std_os::fd::RawFd) {
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
            Err(error) if error.kind() == path_std_io::ErrorKind::WouldBlock => break,
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
enum ShellWaitEvent {
    Exited(std::process::ExitStatus),
    WaitFailed,
    Cancelled,
}

#[cfg(unix)]
fn shell_wait_apply_event(
    event: ShellWaitEvent,
    status: &mut Option<std::process::ExitStatus>,
) -> bool {
    match event {
        ShellWaitEvent::Exited(received) => {
            *status = Some(received);
            true
        }
        ShellWaitEvent::WaitFailed => {
            *status = None;
            true
        }
        ShellWaitEvent::Cancelled => false,
    }
}

#[cfg(unix)]
fn shell_wait_collect_status(
    event_rx: &mpsc::Receiver<ShellWaitEvent>,
    status: &mut Option<std::process::ExitStatus>,
) -> bool {
    use std::sync::mpsc::TryRecvError;

    if status.is_some() {
        return true;
    }
    match event_rx.try_recv() {
        Ok(event) => shell_wait_apply_event(event, status),
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => true,
    }
}

#[cfg(unix)]
fn shell_wait_recv_event_until(
    event_rx: &mpsc::Receiver<ShellWaitEvent>,
    deadline: std::time::Instant,
) -> Option<ShellWaitEvent> {
    let timeout = deadline.saturating_duration_since(path_std_time::Instant::now());
    event_rx.recv_timeout(timeout).ok()
}

#[cfg(unix)]
fn shell_wait_poll_timeout_ms(deadline: std::time::Instant) -> i32 {
    let now = path_std_time::Instant::now();
    if deadline <= now {
        return 0;
    }
    let remaining = deadline - now;
    i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX)
}

#[cfg(unix)]
fn shell_wait_dup_owned_fd(fd: &path_std_os::fd::OwnedFd) -> Option<path_std_os::fd::OwnedFd> {
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
fn shell_wait_write_wake_byte(fd: &path_std_os::fd::OwnedFd) {
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
fn shell_wait_drain_wake_fd(wake_read: &path_std_os::fd::OwnedFd) {
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
    read: path_std_os::fd::OwnedFd,
    /// Write side used by the child waiter thread.
    write: path_std_os::fd::OwnedFd,
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
    /// Nonblocking stdout endpoint owned by the foreground wait loop.
    stdout_pipe: Option<ShellStdout>,
    /// Nonblocking stderr endpoint owned by the foreground wait loop.
    stderr_pipe: Option<ShellStderr>,
    /// PTY user guards retained across child pre-exec on supported targets.
    #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
    output_users: Option<[std::fs::File; 2]>,
    /// Wake fd signalled when the child exits or cancellation arrives.
    wake_read: Option<path_std_os::fd::OwnedFd>,
    /// Receiver for child-exit and cancellation events.
    event_rx: mpsc::Receiver<ShellWaitEvent>,
    /// Cross-thread cancellation flag set by the cancellation waiter.
    cancelled_by_request: std::sync::Arc<path_std_sync::atomic::AtomicBool>,
}

#[cfg(unix)]
impl ShellWaitState {
    fn start(mut process: ShellProcess, cancel_rx: Option<mpsc::Receiver<()>>) -> Self {
        use std::os::fd::AsRawFd;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let pid = process.child.id();
        debug!(
            pid,
            cancel_enabled = cancel_rx.is_some(),
            "starting shell wait state"
        );

        let stdout_pipe = process.stdout.take();
        let stderr_pipe = process.stderr.take();
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
        let (event_tx, event_rx) = mpsc::channel::<ShellWaitEvent>();
        let cancelled_by_request = Arc::new(AtomicBool::new(false));
        spawn_shell_cancel_waiter(
            pid,
            cancel_rx,
            Arc::clone(&cancelled_by_request),
            cancel_wake_write,
            event_tx.clone(),
        );

        let (wake_read, wake_write) = wake_pipe
            .map(|wake_pipe| (Some(wake_pipe.read), Some(wake_pipe.write)))
            .unwrap_or((None, None));
        spawn_shell_child_waiter(pid, process.child, wake_write, waiter_wake_read, event_tx);
        Self {
            pid,
            stdout_pipe,
            stderr_pipe,
            #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
            output_users: process.output_users,
            wake_read,
            event_rx,
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
        let deadline = path_std_time::Instant::now() + timeout;

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

        #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
        drop(self.output_users.take());
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
        if shell_wait_collect_status(&self.event_rx, status) {
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

        let now = path_std_time::Instant::now();
        if deadline <= now {
            debug!(
                pid = self.pid,
                "shell wait loop timed out; killing process group"
            );
            kill_process_group_by_pid(self.pid);
            return ShellWaitPoll::TimedOut;
        }

        if let Some(event) = self.wait_for_io_or_wake(deadline) {
            match event {
                ShellWaitEvent::Exited(received) => {
                    *status = Some(received);
                    debug!(pid = self.pid, status = ?status, "shell wait loop observed child status");
                    return ShellWaitPoll::Finished;
                }
                ShellWaitEvent::WaitFailed => {
                    *status = None;
                    debug!(pid = self.pid, status = ?status, "shell wait loop observed child status");
                    return ShellWaitPoll::Finished;
                }
                ShellWaitEvent::Cancelled => {
                    debug!(
                        pid = self.pid,
                        "shell wait loop observed cancellation; killing process group"
                    );
                    kill_process_group_by_pid(self.pid);
                    return ShellWaitPoll::Cancelled;
                }
            }
        }
        ShellWaitPoll::Continue
    }

    fn drain_after_terminal(
        &mut self,
        output: &mut CapturedOutput,
        status: &mut Option<std::process::ExitStatus>,
    ) {
        let drain_deadline = path_std_time::Instant::now() + SHELL_WAIT_DRAIN_AFTER_DONE;
        loop {
            self.read_available_output(output);
            let _ = shell_wait_collect_status(&self.event_rx, status);
            if self.stdout_pipe.is_none() && self.stderr_pipe.is_none() {
                trace!(pid = self.pid, "shell output drain completed");
                break;
            }
            if drain_deadline <= path_std_time::Instant::now() {
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

    fn wait_for_io_or_wake(&self, deadline: std::time::Instant) -> Option<ShellWaitEvent> {
        let mut poll_fds = self.output_and_wake_poll_fds();
        if poll_fds.is_empty() {
            return shell_wait_recv_event_until(&self.event_rx, deadline);
        }

        shell_wait_poll_fds_until(&mut poll_fds, deadline);
        if let Some(wake_read) = self.wake_read.as_ref() {
            shell_wait_drain_wake_fd(wake_read);
        }
        None
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
    cancelled_by_request: std::sync::Arc<path_std_sync::atomic::AtomicBool>,
    cancel_wake_write: Option<path_std_os::fd::OwnedFd>,
    event_tx: mpsc::Sender<ShellWaitEvent>,
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
            let _ = event_tx.send(ShellWaitEvent::Cancelled);
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
    wake_write: Option<path_std_os::fd::OwnedFd>,
    waiter_wake_read: Option<path_std_os::fd::OwnedFd>,
    event_tx: mpsc::Sender<ShellWaitEvent>,
) {
    let _waiter = std::thread::spawn(move || {
        // Keep a duplicate read end alive until the waiter publishes status and
        // writes the final wake byte. Otherwise a foreground loop that returns
        // early after timeout/cancellation could drop the last reader first,
        // turning this best-effort wake into an avoidable no-reader write.
        let _wake_read_guard = waiter_wake_read;
        let status = child.wait();
        debug!(pid, status = ?status, "shell child waiter finished");
        match status {
            Ok(status) => {
                let _ = event_tx.send(ShellWaitEvent::Exited(status));
            }
            Err(_) => {
                let _ = event_tx.send(ShellWaitEvent::WaitFailed);
            }
        }
        if let Some(wake_write) = wake_write {
            shell_wait_write_wake_byte(&wake_write);
        }
    });
}

#[cfg(unix)]
fn wait_with_timeout(
    child: ShellProcess,
    timeout: std::time::Duration,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> WaitResult {
    // On Unix the shell tool must not wait for stdout/stderr EOF: background
    // or detached descendants can retain output endpoints long after the
    // foreground shell exits or is killed. The wait state therefore polls
    // nonblocking outputs and an internal child-exit wake pipe together, then
    // returns after foreground exit or timeout with only a brief nonblocking
    // drain.
    ShellWaitState::start(child, cancel_rx).wait(timeout)
}

/// Wait for a child process with a timeout, preserving output even when
/// the timeout is reached.
///
/// Non-Unix waits for child/cancel/deadline events without fixed polling and
/// performs only a bounded drain after the child reaches a terminal state.
#[cfg(not(unix))]
fn wait_with_timeout(
    mut process: ShellProcess,
    timeout: std::time::Duration,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> WaitResult {
    let stdout_pipe = process.stdout.take();
    let stderr_pipe = process.stderr.take();

    let output = path_std_sync::Arc::new(path_std_sync::Mutex::new(CapturedOutput::default()));
    let stop_pipe_readers = path_std_sync::Arc::new(path_std_sync_atomic::AtomicBool::new(false));
    let pipe_done_rx =
        spawn_nonunix_shell_pipe_captures(stdout_pipe, stderr_pipe, &output, &stop_pipe_readers);

    let child_wait = NonUnixChildWait::start(&process.child, timeout, cancel_rx);
    let mut timed_out = false;
    let mut cancelled = false;
    let status = match child_wait.recv() {
        NonUnixChildEvent::Exited => process.child.wait().ok(),
        NonUnixChildEvent::Cancelled => match process.child.try_wait() {
            Ok(Some(status)) => Some(status),
            _ => {
                cancelled = true;
                let _ = process.child.kill();
                process.child.wait().ok()
            }
        },
        NonUnixChildEvent::TimedOut => match process.child.try_wait() {
            Ok(Some(status)) => Some(status),
            _ => {
                timed_out = true;
                let _ = process.child.kill();
                process.child.wait().ok()
            }
        },
        NonUnixChildEvent::WaitFailed => {
            let _ = process.child.kill();
            process.child.wait().ok()
        }
    };
    child_wait.join_exit_and_timeout_watchers();
    drain_nonunix_pipe_captures(&pipe_done_rx, NON_UNIX_PIPE_CAPTURE_COUNT);
    stop_pipe_readers.store(true, path_std_sync_atomic::Ordering::SeqCst);

    let mut output = std::mem::take(&mut *output.lock().unwrap_or_else(|error| error.into_inner()));
    output.finish();
    wait_result_from_parts(status, timed_out, cancelled, output)
}

#[cfg(not(unix))]
fn spawn_nonunix_shell_pipe_captures(
    stdout_pipe: Option<impl std::io::Read + Send + 'static>,
    stderr_pipe: Option<impl std::io::Read + Send + 'static>,
    output: &std::sync::Arc<std::sync::Mutex<CapturedOutput>>,
    stop: &std::sync::Arc<path_std_sync::atomic::AtomicBool>,
) -> mpsc::Receiver<()> {
    let (done_tx, done_rx) = mpsc::channel();
    spawn_nonunix_shell_pipe_capture(
        stdout_pipe,
        OutputStream::Stdout,
        path_std_sync::Arc::clone(output),
        path_std_sync::Arc::clone(stop),
        done_tx.clone(),
    );
    spawn_nonunix_shell_pipe_capture(
        stderr_pipe,
        OutputStream::Stderr,
        path_std_sync::Arc::clone(output),
        path_std_sync::Arc::clone(stop),
        done_tx,
    );
    done_rx
}

#[cfg(not(unix))]
fn spawn_nonunix_shell_pipe_capture(
    pipe: Option<impl std::io::Read + Send + 'static>,
    stream: OutputStream,
    output: std::sync::Arc<std::sync::Mutex<CapturedOutput>>,
    stop: std::sync::Arc<path_std_sync::atomic::AtomicBool>,
    done_tx: mpsc::Sender<()>,
) {
    let Some(mut pipe) = pipe else {
        let _ = done_tx.send(());
        return;
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; SHELL_WAIT_READ_CHUNK_BYTES];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stop.load(path_std_sync_atomic::Ordering::SeqCst) {
                        break;
                    }
                    output
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push_bytes(stream, &buf[..n]);
                }
            }
        }
        let _ = done_tx.send(());
    });
}

#[cfg(not(unix))]
fn drain_nonunix_pipe_captures(done_rx: &mpsc::Receiver<()>, pipe_count: usize) {
    let deadline = path_std_time::Instant::now() + NON_UNIX_PIPE_DRAIN_AFTER_DONE;
    let mut closed = 0;
    while closed < pipe_count {
        let now = path_std_time::Instant::now();
        if deadline <= now {
            break;
        }
        match done_rx.recv_timeout(deadline - now) {
            Ok(()) => closed += 1,
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
}

#[cfg(not(unix))]
enum NonUnixChildEvent {
    Exited,
    Cancelled,
    TimedOut,
    WaitFailed,
}

#[cfg(not(unix))]
// Coordinates non-Unix process completion without taking ownership of `Child`.
// The foreground caller keeps the only `Child`, so it can call `kill()` and
// `wait()` on cancellation or timeout. Watchers only report child-exit,
// cancellation, and deadline events; the cancel watcher is intentionally
// detached because `std::sync::mpsc::Receiver` cannot be woken by the stopper
// channel used for the timeout watcher.
struct NonUnixChildWait {
    event_rx: mpsc::Receiver<NonUnixChildEvent>,
    exit_handle: std::thread::JoinHandle<()>,
    timeout_stop_tx: mpsc::Sender<()>,
    timeout_handle: std::thread::JoinHandle<()>,
}

#[cfg(not(unix))]
impl NonUnixChildWait {
    fn start(
        child: &std::process::Child,
        timeout: std::time::Duration,
        cancel_rx: Option<mpsc::Receiver<()>>,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let (timeout_stop_tx, timeout_stop_rx) = mpsc::channel();
        let exit_handle = Self::spawn_exit_watcher(child, event_tx.clone());
        let timeout_handle = std::thread::spawn({
            let event_tx = event_tx.clone();
            move || match timeout_stop_rx.recv_timeout(timeout) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = event_tx.send(NonUnixChildEvent::TimedOut);
                }
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
        });
        if let Some(cancel_rx) = cancel_rx {
            let _cancel_handle = std::thread::spawn(move || {
                if cancel_rx.recv().is_ok() {
                    let _ = event_tx.send(NonUnixChildEvent::Cancelled);
                }
            });
        }

        Self {
            event_rx,
            exit_handle,
            timeout_stop_tx,
            timeout_handle,
        }
    }

    fn recv(&self) -> NonUnixChildEvent {
        self.event_rx
            .recv()
            .unwrap_or(NonUnixChildEvent::WaitFailed)
    }

    fn join_exit_and_timeout_watchers(self) {
        let _ = self.timeout_stop_tx.send(());
        let _ = self.timeout_handle.join();
        let _ = self.exit_handle.join();
    }

    #[cfg(windows)]
    fn spawn_exit_watcher(
        child: &std::process::Child,
        event_tx: mpsc::Sender<NonUnixChildEvent>,
    ) -> std::thread::JoinHandle<()> {
        use std::os::windows::io::AsRawHandle;

        const INFINITE: u32 = u32::MAX;
        const WAIT_FAILED: u32 = u32::MAX;

        // SAFETY: the raw process handle is borrowed from `Child`; the caller
        // keeps the `Child` alive and joins this watcher before dropping it.
        #[allow(unsafe_code)]
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
        }

        let handle = child.as_raw_handle() as usize;
        std::thread::spawn(move || {
            // SAFETY: `handle` is a borrowed process handle that remains valid
            // until this watcher is joined before the owning `Child` is dropped.
            #[allow(unsafe_code)]
            unsafe {
                let result = WaitForSingleObject(handle as *mut std::ffi::c_void, INFINITE);
                let event = if result == WAIT_FAILED {
                    NonUnixChildEvent::WaitFailed
                } else {
                    NonUnixChildEvent::Exited
                };
                let _ = event_tx.send(event);
            }
        })
    }

    #[cfg(all(not(unix), not(windows)))]
    fn spawn_exit_watcher(
        _child: &std::process::Child,
        event_tx: mpsc::Sender<NonUnixChildEvent>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let _ = event_tx.send(NonUnixChildEvent::WaitFailed);
        })
    }
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
        retained_prefix: String,
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
    /// Incremental stdout decoder.
    stdout: StreamDecoder,
    /// Incremental stderr decoder.
    stderr: StreamDecoder,
    /// Leading lines retained for model rendering.
    head_lines: Vec<OutputLine>,
    /// Trailing lines retained for model rendering.
    tail_lines: Vec<OutputLine>,
    /// Complete rendered line count.
    total_lines: usize,
    /// Complete rendered byte count.
    total_bytes: usize,
    /// Whether a line exceeded the saved-output bound.
    saw_truncated_line: bool,
    /// Ordered rendering retained for the artifact.
    saved_output: String,
    /// Whether the artifact rendering is incomplete.
    saved_output_incomplete: bool,
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
        let mut line = OutputLine { stream, content };
        let full_line = render_saved_output_line(&line);
        let formatted_len = match &line.content {
            OutputContent::Truncated {
                invalid_utf8,
                ending,
                original_text_bytes,
                ..
            } => {
                let mut markers = Vec::new();
                if *invalid_utf8 {
                    markers.push("invalid-utf8");
                }
                if let Some(marker) = line_ending_marker(*ending) {
                    markers.push(marker);
                }
                format_output_line(
                    stream.prefix(),
                    (!markers.is_empty()).then(|| markers.join(",")).as_deref(),
                    "",
                )
                .len()
                .saturating_add(*original_text_bytes)
            }
            _ => full_line.len(),
        };
        let separator_bytes = usize::from(self.total_lines != 0);
        self.total_bytes += separator_bytes + formatted_len;
        let source_line_truncated = matches!(line.content, OutputContent::Truncated { .. });
        if source_line_truncated {
            self.saw_truncated_line = true;
        }
        if !self.saved_output_incomplete {
            let separator_bytes = usize::from(!self.saved_output.is_empty());
            let remaining = MAX_SAVED_OUTPUT_BYTES.saturating_sub(self.saved_output.len());
            if separator_bytes.saturating_add(full_line.len()) <= remaining {
                if separator_bytes != 0 {
                    self.saved_output.push('\n');
                }
                self.saved_output.push_str(&full_line);
            } else {
                self.saved_output_incomplete = true;
                const MARKER: &str = "...(saved output truncated)";
                let marker_bytes = separator_bytes.saturating_add(MARKER.len());
                if marker_bytes <= remaining {
                    if separator_bytes != 0 {
                        self.saved_output.push('\n');
                    }
                    self.saved_output.push_str(MARKER);
                }
            }
        }
        if source_line_truncated {
            self.saved_output_incomplete = true;
        }
        if MAX_MODEL_SHELL_OUTPUT_BYTES < formatted_len {
            let content = std::mem::replace(
                &mut line.content,
                OutputContent::Text {
                    text: String::new(),
                    ending: None,
                },
            );
            let (invalid_utf8, ending, original_text_bytes) = match content {
                OutputContent::Text { text, ending } => (false, ending, text.len()),
                OutputContent::InvalidUtf8 { text, ending } => (true, ending, text.len()),
                OutputContent::Truncated {
                    invalid_utf8,
                    ending,
                    original_text_bytes,
                    ..
                } => (invalid_utf8, ending, original_text_bytes),
            };
            line.content = OutputContent::Truncated {
                invalid_utf8,
                ending,
                original_text_bytes,
                retained_prefix: String::new(),
            };
        }
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

    fn truncate(&self) -> crate::truncate::Truncated {
        let mut rendered = self
            .head_lines
            .iter()
            .map(render_output_line)
            .collect::<Vec<_>>();
        rendered.extend(self.tail_lines.iter().map(render_output_line));
        let rendered_refs = rendered.iter().map(String::as_str).collect::<Vec<_>>();
        truncate_line_oriented_lines_with_byte_limit(
            rendered_refs.iter().copied(),
            self.total_lines,
            if self.saw_truncated_line {
                self.total_bytes.max(MAX_MODEL_SHELL_OUTPUT_BYTES + 1)
            } else {
                self.total_bytes
            },
            MAX_MODEL_SHELL_OUTPUT_BYTES,
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
                        } else {
                            self.pending_line_original_bytes = self
                                .pending_line_original_bytes
                                .saturating_add('\u{fffd}'.len_utf8());
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
            } else {
                self.pending_line_original_bytes = self
                    .pending_line_original_bytes
                    .saturating_add('\u{fffd}'.len_utf8());
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
            return OutputContent::Truncated {
                invalid_utf8,
                ending,
                original_text_bytes,
                retained_prefix: std::mem::take(&mut self.pending_line),
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

fn render_saved_output_line(line: &OutputLine) -> String {
    let mut rendered = render_output_line(line);
    if let OutputContent::Truncated {
        retained_prefix, ..
    } = &line.content
    {
        rendered.push_str(retained_prefix);
    }
    rendered
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

fn command_display(command: &str) -> (String, Option<ToolUsePayload>) {
    let mut lines = command.lines();
    let first_line = lines.next().unwrap_or_default();
    let args = shorten_command_line(first_line);
    let payload = (lines.next().is_some() || args != first_line).then(|| ToolUsePayload::Text {
        text: command.to_owned(),
    });
    (args, payload)
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

pub(crate) struct CommandDetails {
    /// Process exit status, when available.
    pub(crate) status: Option<i32>,
    /// Terminating signal, when available.
    pub(crate) signal: Option<i32>,
    /// Whether the deadline terminated the command.
    pub(crate) timed_out: bool,
    /// Approximate duration for slow commands.
    pub(crate) duration_seconds: Option<u64>,
    /// Machine-readable process termination classification.
    pub(crate) termination_reason: &'static str,
    /// Complete line count when model output was truncated.
    pub(crate) total_lines: Option<usize>,
    /// Complete byte count when model output was truncated.
    pub(crate) total_bytes: Option<usize>,
    /// Bounded model-visible ordered rendering.
    pub(crate) output: String,
    /// Whether model-visible rendering was truncated.
    pub(crate) truncated: bool,
    /// Whether all captured bytes were valid UTF-8.
    pub(crate) valid_utf8: bool,
    /// Ephemeral artifact metadata for truncated output.
    pub(crate) saved_output: Option<SavedArtifact>,
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
        saved_output,
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
        entries.push((
            CborValue::Text("truncation_warning".to_owned()),
            CborValue::Text(
                "Fetching excessive output is inefficient; prefer narrower commands or filters."
                    .to_owned(),
            ),
        ));
        match &saved_output {
            Some(SavedArtifact::Available(saved_output)) => entries.push((
                CborValue::Text(
                    if saved_output.incomplete {
                        "saved_output_path"
                    } else {
                        "full_output_path"
                    }
                    .to_owned(),
                ),
                CborValue::Text(
                    saved_output
                        .path
                        .to_str()
                        .expect("spool accepts only safe UTF-8 paths")
                        .to_owned(),
                ),
            )),
            Some(SavedArtifact::Unavailable) => entries.push((
                CborValue::Text("saved_output_unavailable".to_owned()),
                CborValue::Bool(true),
            )),
            None => {}
        }
        if let Some(SavedArtifact::Available(saved_output)) = &saved_output
            && saved_output.incomplete
        {
            entries.push((
                CborValue::Text("saved_output_truncated".to_owned()),
                CborValue::Bool(true),
            ));
            entries.push((
                CborValue::Text("saved_output_bytes".to_owned()),
                CborValue::Integer((saved_output.saved_bytes as i64).into()),
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

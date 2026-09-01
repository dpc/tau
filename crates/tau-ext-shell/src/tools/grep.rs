//! `grep` tool: ripgrep-backed search using `rg --json`.

use std::{
    io as path_std_io, os as path_std_os, process as path_std_process, time as path_std_time,
};

use base64::engine as path_base64_engine;

#[cfg(test)]
mod tests;
use std::ffi::OsString;
use std::fmt;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use tau_proto::CborValue;

use crate::argument::{
    argument_text, optional_argument_bool, optional_argument_int_strict, optional_argument_text,
};
use crate::display::{ToolFailure, ToolOutput, text_stats};
use crate::isolation::apply_command_isolation;
use crate::tools::CancellableToolRun;
use crate::tools::find::{escape_path_text, render_path_bytes};
use crate::truncate::{MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, truncate_head};

pub(crate) const DEFAULT_GREP_LIMIT: usize = 100;
pub(crate) const GREP_MAX_LINE_LENGTH: usize = 500;
const MAX_GREP_LIMIT: usize = MAX_OUTPUT_LINES;
const MAX_GREP_CONTEXT: usize = 20;

pub(crate) fn run_grep(arguments: &CborValue) -> Result<ToolOutput, ToolFailure> {
    match run_grep_cancellable(arguments, None)? {
        CancellableToolRun::Finished(output) => Ok(*output),
        CancellableToolRun::Cancelled => Err(ToolFailure::new("cancelled")),
    }
}

pub(crate) fn run_grep_cancellable(
    arguments: &CborValue,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<CancellableToolRun, ToolFailure> {
    let options = GrepOptions::parse(arguments)?;
    let display_args = options.display_args();
    let with_args = |f: ToolFailure| f.with_args(display_args.clone());

    let GrepProcessOutput {
        stream,
        status,
        stderr,
        cancelled,
    } = run_ripgrep(&options, cancel_rx).map_err(with_args)?;

    if cancelled {
        return Ok(CancellableToolRun::Cancelled);
    }

    // rg exit codes: 0=matches found, 1=no matches, 2=error.
    // Exit-2 is overloaded — ripgrep emits regex parse errors, IO
    // errors, and permission denials all under the same code. Classify
    // the stderr into a short, single-line message so the UI doesn't
    // surface a multi-line regex-parser dump in the inline tool block.
    if status == Some(2) {
        let stderr_raw = String::from_utf8_lossy(&stderr);
        return Err(with_args(ToolFailure::from(
            classify_ripgrep_stderr(stderr_raw.trim()).to_string(),
        )));
    }

    Ok(CancellableToolRun::Finished(Box::new(render_grep_output(
        stream,
        status,
        display_args,
        options.limit,
    ))))
}

/// Parsed model-facing grep arguments after validation and defaults.
struct GrepOptions {
    /// Search-pattern mode and text passed to ripgrep after the `--` separator.
    pattern: GrepPattern,
    /// Optional user-supplied search root; defaults to the current directory.
    path: Option<PathBuf>,
    /// Optional ripgrep glob filter passed as `--glob`.
    glob: Option<String>,
    /// Whether matching should ignore case.
    ignore_case: bool,
    /// Optional number of context lines requested around each match.
    context: Option<usize>,
    /// Maximum number of match records to render before stopping ripgrep.
    limit: usize,
}

/// A grep pattern with its explicit ripgrep matching mode.
enum GrepPattern {
    /// Match the pattern text as a fixed string.
    Literal(String),
    /// Interpret the pattern text as a regular expression.
    Regex(String),
}

impl GrepPattern {
    fn text(&self) -> &str {
        match self {
            Self::Literal(text) | Self::Regex(text) => text,
        }
    }

    fn push_ripgrep_args(&self, args: &mut Vec<OsString>) {
        if matches!(self, Self::Literal(_)) {
            args.push("--fixed-strings".into());
        }
    }
}

impl GrepOptions {
    fn parse(arguments: &CborValue) -> Result<Self, ToolFailure> {
        let pattern = argument_text(arguments, "pattern")?;
        let path = optional_argument_text(arguments, "path")?.map(PathBuf::from);
        let glob = optional_argument_text(arguments, "glob")?;
        let ignore_case = optional_bool_argument(arguments, "ignoreCase")?;
        // Literal matching is the default. Most callers are searching for
        // an exact string and regex metacharacters in that string (`[`,
        // `(`, `.`, `?`, `+`, `*`, `|`, `{`, `\`) would otherwise either
        // fail to parse or silently match something unintended. Regex
        // users opt in explicitly with `regex: true`.
        let pattern = match optional_bool_argument(arguments, "regex")? {
            true => GrepPattern::Regex(pattern),
            false => GrepPattern::Literal(pattern),
        };
        let context =
            optional_bounded_usize_argument(arguments, "context", 0, MAX_GREP_CONTEXT, None)?;
        let limit = optional_bounded_usize_argument(
            arguments,
            "limit",
            1,
            MAX_GREP_LIMIT,
            Some(DEFAULT_GREP_LIMIT),
        )?
        .expect("defaulted limit must be present");

        Ok(Self {
            pattern,
            path,
            glob,
            ignore_case,
            context,
            limit,
        })
    }

    fn search_path(&self) -> &Path {
        self.path.as_deref().unwrap_or_else(|| Path::new("."))
    }

    fn display_args(&self) -> String {
        match self.glob.as_deref() {
            Some(g) => format!(
                "{:?} in {} [{g}]",
                self.pattern.text(),
                self.search_path().display()
            ),
            None => format!(
                "{:?} in {}",
                self.pattern.text(),
                self.search_path().display()
            ),
        }
    }

    fn ripgrep_args(&self) -> Vec<OsString> {
        // Use `--json` for structured output. This replaces the previous
        // hand-rolled `PATH:LINE:CONTENT` vs `PATH-LINE-CONTENT` line
        // classifier, which had a known misclassification mode on paths
        // like `file-12-34.txt`. The JSON envelope cleanly separates
        // match from context records.
        //
        // `--json` always includes the `path` field, even for a single file
        // and regardless of `--with-filename`, so the renderer can emit a
        // per-file path heading in all cases; `--with-filename` is kept
        // defensively for older ripgrep behavior. `--heading` is
        // deliberately not passed: it only affects rg's human-readable
        // output and is a no-op under `--json`, so the heading grouping is
        // done by the renderer below.
        let mut args: Vec<OsString> = vec![
            "--json".into(),
            "--hidden".into(),
            "--with-filename".into(),
            "--max-columns".into(),
            GREP_MAX_LINE_LENGTH.to_string().into(),
            "--max-columns-preview".into(),
        ];
        self.push_optional_ripgrep_args(&mut args);
        args.push("--".into());
        args.push(self.pattern.text().into());
        args.push(self.search_path().as_os_str().to_owned());
        args
    }

    fn push_optional_ripgrep_args(&self, args: &mut Vec<OsString>) {
        if self.ignore_case {
            args.push("--ignore-case".into());
        }
        self.pattern.push_ripgrep_args(args);
        if let Some(glob) = &self.glob {
            args.push("--glob".into());
            args.push(glob.into());
        }
        if let Some(context) = self.context {
            args.push(format!("--context={context}").into());
        }
    }
}

fn optional_bool_argument(arguments: &CborValue, name: &str) -> Result<bool, ToolFailure> {
    Ok(optional_argument_bool(arguments, name)
        .map_err(ToolFailure::from)?
        .unwrap_or(false))
}

fn optional_bounded_usize_argument(
    arguments: &CborValue,
    name: &str,
    min: usize,
    max: usize,
    default: Option<usize>,
) -> Result<Option<usize>, ToolFailure> {
    let Some(value) = optional_argument_int_strict(arguments, name).map_err(ToolFailure::from)?
    else {
        return Ok(default);
    };
    let min_i64 = i64::try_from(min).expect("grep bounds fit in i64");
    if value < min_i64 {
        return Err(ToolFailure::new(format!("{name} must be >= {min}")));
    }
    let value =
        usize::try_from(value).map_err(|_| ToolFailure::new(format!("{name} is too large")))?;
    if max < value {
        return Err(ToolFailure::new(format!("{name} must be <= {max}")));
    }
    Ok(Some(value))
}

struct GrepProcessOutput {
    /// Rendered stream records and truncation/limit metadata.
    stream: GrepStreamResult,
    /// Process exit status code, or `None` if ripgrep was signal-terminated.
    status: Option<i32>,
    /// Bounded stderr bytes captured for exit-code classification.
    stderr: Vec<u8>,
    /// Whether a cancellation request terminated ripgrep before completion.
    cancelled: bool,
}

fn run_ripgrep(
    options: &GrepOptions,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<GrepProcessOutput, ToolFailure> {
    if cancel_rx.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
        return Ok(GrepProcessOutput {
            stream: GrepStreamResult {
                result_lines: Vec::new(),
                match_count: 0,
                lines_truncated: false,
                match_limit_reached: false,
            },
            status: None,
            stderr: Vec::new(),
            cancelled: true,
        });
    }

    let mut cmd = Command::new("rg");
    cmd.args(options.ripgrep_args())
        .stdout(path_std_process::Stdio::piped())
        .stderr(path_std_process::Stdio::piped());
    apply_command_isolation(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolFailure::from(format!("failed to start ripgrep: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolFailure::from("ripgrep stdout pipe missing".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolFailure::from("ripgrep stderr pipe missing".to_owned()))?;
    let stderr_handle = std::thread::spawn(move || read_limited_bytes(stderr, MAX_OUTPUT_BYTES));
    let (stop_tx, stop_rx) = mpsc::channel();
    let wait_handle = std::thread::spawn(move || wait_ripgrep(child, stop_rx, cancel_rx));

    let stream = read_grep_json(stdout, options.limit);

    // If the limit fired we may have killed reading mid-stream; make
    // sure the child does not linger.
    if stream.match_limit_reached {
        let _ = stop_tx.send(());
    }

    let wait = wait_handle
        .join()
        .map_err(|_| ToolFailure::from("ripgrep waiter thread panicked".to_owned()))?;
    let stderr = stderr_handle.join().unwrap_or_default();
    let (exit_status, cancelled) = wait?;

    Ok(GrepProcessOutput {
        stream,
        status: exit_status.and_then(|status| status.code()),
        stderr,
        cancelled,
    })
}

#[cfg(target_os = "linux")]
enum RipgrepWaitEvent {
    Exited,
    ExitWaitFailed(String),
    Cancelled,
    MatchLimitReached,
}

/// Wait for ripgrep to exit while reacting to cancellation and match limits.
///
/// The Linux implementation uses short helper threads only to bridge blocking
/// notifications into one coordinator channel: a pidfd waiter for child-exit
/// readiness, a match-limit listener for `stop_rx`, and an optional
/// cancellation listener. The stop/cancel listeners are intentionally unjoined;
/// they exit when their channel is signalled or dropped, and failed sends only
/// mean the coordinator already returned. The pidfd waiter exits after child
/// readiness. The coordinator owns and reaps `Child`, which avoids pid reuse
/// races; cancellation returns `cancelled = true`, while a match-limit stop
/// only terminates ripgrep so the partial grep output can be returned normally.
/// If pidfds are unavailable, or on non-Linux builds where
/// `std::process::Child` has no portable readiness primitive, this falls back
/// to polling to preserve cancellation and match-limit behavior. That fallback
/// is intentionally scoped as a compatibility path; the non-polling guarantee
/// applies to the normal Linux path used by current CI and production
/// development.
fn wait_ripgrep(
    child: std::process::Child,
    stop_rx: mpsc::Receiver<()>,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<(Option<std::process::ExitStatus>, bool), ToolFailure> {
    #[cfg(target_os = "linux")]
    {
        wait_ripgrep_linux(child, stop_rx, cancel_rx)
    }
    #[cfg(not(target_os = "linux"))]
    {
        wait_ripgrep_polling_fallback(child, stop_rx, cancel_rx)
    }
}

/// Coordinate ripgrep child exit, cancellation, and match-limit stops without
/// polling. A pidfd waiter reports child-exit readiness without reaping the
/// child, while stop/cancel listener threads block until their channels are
/// signalled or closed. The coordinator keeps ownership of `Child`, so
/// cancellation and match-limit termination use the live child handle and then
/// reap it directly; only cancellation sets the returned `cancelled` flag.
#[cfg(target_os = "linux")]
fn wait_ripgrep_linux(
    mut child: std::process::Child,
    stop_rx: mpsc::Receiver<()>,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<(Option<std::process::ExitStatus>, bool), ToolFailure> {
    let pid = child.id();
    let (event_tx, event_rx) = mpsc::channel();
    if spawn_ripgrep_exit_waiter(pid, event_tx.clone()).is_err() {
        return wait_ripgrep_polling_fallback(child, stop_rx, cancel_rx);
    }

    let stop_tx = event_tx.clone();
    std::thread::spawn(move || {
        if stop_rx.recv().is_ok() {
            let _ = stop_tx.send(RipgrepWaitEvent::MatchLimitReached);
        }
    });

    if let Some(cancel_rx) = cancel_rx {
        let cancel_tx = event_tx;
        std::thread::spawn(move || {
            if cancel_rx.recv().is_ok() {
                let _ = cancel_tx.send(RipgrepWaitEvent::Cancelled);
            }
        });
    }

    let mut cancelled = false;
    match event_rx.recv() {
        Ok(RipgrepWaitEvent::Exited) => {
            let status = child.wait().map_err(|error| {
                ToolFailure::from(format!("failed to wait for ripgrep: {error}"))
            })?;
            Ok((Some(status), cancelled))
        }
        Ok(RipgrepWaitEvent::ExitWaitFailed(error)) => {
            kill_ripgrep_child(&mut child, pid);
            let _ = child.wait();
            Err(ToolFailure::from(format!(
                "failed to wait for ripgrep exit readiness: {error}"
            )))
        }
        Ok(RipgrepWaitEvent::Cancelled) => {
            cancelled = true;
            kill_ripgrep_child(&mut child, pid);
            let status = child.wait().ok();
            Ok((status, cancelled))
        }
        Ok(RipgrepWaitEvent::MatchLimitReached) => {
            kill_ripgrep_child(&mut child, pid);
            let status = child.wait().ok();
            Ok((status, cancelled))
        }
        Err(_) => Ok((None, cancelled)),
    }
}

#[cfg(target_os = "linux")]
fn spawn_ripgrep_exit_waiter(
    pid: u32,
    event_tx: mpsc::Sender<RipgrepWaitEvent>,
) -> Result<(), ToolFailure> {
    use std::os::fd::AsRawFd;

    let pidfd = open_pidfd(pid)?;
    std::thread::spawn(move || {
        let mut poll_fd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: `poll_fd` points at one valid `pollfd` backed by an owned
            // pidfd that remains alive for the duration of this thread.
            #[allow(unsafe_code)]
            let result = unsafe { libc::poll(&mut poll_fd, 1, -1) };
            if result < 0 {
                let error = path_std_io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                let _ = event_tx.send(RipgrepWaitEvent::ExitWaitFailed(error.to_string()));
                return;
            }
            if result == 0 {
                continue;
            }
            if poll_fd.revents & libc::POLLIN != 0 {
                let _ = event_tx.send(RipgrepWaitEvent::Exited);
                return;
            }
            if poll_fd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                let _ = event_tx.send(RipgrepWaitEvent::ExitWaitFailed(format!(
                    "pidfd poll failed with revents={}",
                    poll_fd.revents
                )));
                return;
            }
        }
    });
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> Result<path_std_os::fd::OwnedFd, ToolFailure> {
    use std::os::fd::{FromRawFd, OwnedFd};

    // SAFETY: `pidfd_open` is called with a pid returned by `Child::id` and no
    // flags; on success it returns a new fd owned by this process.
    #[allow(unsafe_code)]
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(ToolFailure::from(format!(
            "failed to open ripgrep pidfd: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: `fd` was returned by `pidfd_open` above and is uniquely owned here.
    #[allow(unsafe_code)]
    Ok(unsafe { OwnedFd::from_raw_fd(fd as path_std_os::fd::RawFd) })
}

#[cfg(target_os = "linux")]
fn kill_ripgrep_child(child: &mut std::process::Child, pid: u32) {
    // Production grep children run under `apply_command_isolation`, which makes
    // the child pid the process-group id. Kill the group first so descendants do
    // not linger, then kill through the still-owned child handle as a pid-safe
    // fallback for tests or isolation failure.
    // SAFETY: The negative pid targets the process group created for this child
    // by `apply_command_isolation`; errors are ignored because the child may have
    // exited already and `Child::kill` below is the pid-safe fallback.
    #[allow(unsafe_code)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    let _ = child.kill();
}

fn wait_ripgrep_polling_fallback(
    mut child: std::process::Child,
    stop_rx: mpsc::Receiver<()>,
    cancel_rx: Option<mpsc::Receiver<()>>,
) -> Result<(Option<std::process::ExitStatus>, bool), ToolFailure> {
    let mut cancelled = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok((Some(status), cancelled)),
            Ok(None) => {}
            Err(error) => {
                return Err(ToolFailure::from(format!(
                    "failed to wait for ripgrep: {error}"
                )));
            }
        }
        if cancel_rx.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
            cancelled = true;
            let _ = child.kill();
            return Ok((child.wait().ok(), cancelled));
        }
        if stop_rx.try_recv().is_ok() {
            let _ = child.kill();
            return Ok((child.wait().ok(), cancelled));
        }
        std::thread::sleep(path_std_time::Duration::from_millis(20));
    }
}

fn render_grep_output(
    stream: GrepStreamResult,
    status: Option<i32>,
    display_args: String,
    limit: usize,
) -> ToolOutput {
    let GrepStreamResult {
        result_lines,
        match_count,
        lines_truncated,
        match_limit_reached,
    } = stream;

    if result_lines.is_empty() {
        let mut display = crate::display::ok_display(display_args.clone());
        display.stats.matches = Some(0);
        return ToolOutput {
            result: grep_result_map(status, 0, "no matches found".to_owned()),
            provider_content: Vec::new(),
            display,
        };
    }

    let total_output_lines = result_lines.len();
    let full_output_text = result_lines.join("\n");

    // Apply byte-level truncation to the assembled output.
    let byte_truncated = truncate_head(&full_output_text);
    let mut output_text = if byte_truncated.was_truncated {
        byte_truncated.content
    } else {
        full_output_text.clone()
    };

    // Build notices.
    let mut notices = Vec::new();
    if match_limit_reached {
        notices.push(limit_reached_notice(limit));
    }
    if byte_truncated.was_truncated {
        notices.push("10 KiB visible output limit reached.".to_owned());
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines."
        ));
    }

    output_text = append_notices_within_cap(output_text, &notices);

    let mut display = crate::display::ok_display(display_args);
    display.stats = text_stats(&output_text);
    display.stats.matches = Some(match_count as u64);
    let mut result = grep_result_map(status, match_count, output_text);
    if byte_truncated.was_truncated
        && let CborValue::Map(entries) = &mut result
    {
        entries.push((
            CborValue::Text("truncated".to_owned()),
            CborValue::Bool(true),
        ));
        entries.push((
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer((total_output_lines as i64).into()),
        ));
        entries.push((
            CborValue::Text("total_bytes".to_owned()),
            CborValue::Integer((full_output_text.len() as i64).into()),
        ));
        crate::shell_output_spool::append_metadata(entries, &full_output_text);
    }
    ToolOutput {
        result,
        provider_content: Vec::new(),
        display,
    }
}

fn limit_reached_notice(limit: usize) -> String {
    if MAX_GREP_LIMIT <= limit {
        format!("{limit} matches limit reached. Maximum limit reached; refine pattern.")
    } else {
        format!(
            "{limit} matches limit reached. Use limit={} for more, or refine pattern.",
            (limit * 2).min(MAX_GREP_LIMIT)
        )
    }
}

fn read_limited_bytes(mut reader: impl Read, limit: usize) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if output.len() < limit {
                    let remaining = limit - output.len();
                    output.extend_from_slice(&buf[..n.min(remaining)]);
                }
            }
        }
    }
    output
}

/// Categorized ripgrep failure (exit code 2). The variants encode the
/// kind of fault; the `Display` impl produces the short single-line
/// message we surface as the tool error. Untagged callers stringify
/// this via `to_string()`. When the unified tool-usage descriptor
/// lands, the variants can be mapped to its `status` field directly
/// instead of being flattened to a string.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RipgrepError {
    /// Bad regex / pattern from the agent. Carries ripgrep's trailing
    /// `error: <diagnostic>` line (e.g. `unclosed group`) when found.
    Usage {
        detail: String,
    },
    NotFound,
    Permission,
    /// Anything else. Carries the first non-empty stderr line so the
    /// chip stays readable but we don't lose the signal entirely.
    Runtime {
        detail: String,
    },
}

impl fmt::Display for RipgrepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage { detail } if !detail.is_empty() => {
                write!(f, "regex parse error: {detail}")
            }
            Self::Usage { .. } => f.write_str("regex parse error"),
            Self::NotFound => f.write_str("no such file or directory"),
            Self::Permission => f.write_str("permission denied"),
            Self::Runtime { detail } if !detail.is_empty() => {
                write!(f, "ripgrep error: {detail}")
            }
            Self::Runtime { .. } => f.write_str("ripgrep error"),
        }
    }
}

/// Classify ripgrep's stderr (exit code 2). ripgrep prints stable,
/// well-known prefixes for each failure class — `regex parse error:`
/// for a bad pattern from the agent, and the OS-error suffix
/// (`(os error 2)` / `(os error 13)`) for not-found and
/// permission-denied — so we can label these without parsing
/// arbitrary downstream text.
pub(crate) fn classify_ripgrep_stderr(stderr: &str) -> RipgrepError {
    if stderr.contains("regex parse error")
        || stderr.contains("error parsing regex")
        || stderr.contains("unrecognized escape sequence")
    {
        // ripgrep's regex-parser output puts the human-readable
        // diagnostic on a trailing `error: <text>` line; the header
        // and pattern/caret lines aren't useful for a one-line chip.
        let detail = stderr
            .lines()
            .filter_map(|l| l.trim().strip_prefix("error:"))
            .map(str::trim)
            .next_back()
            .unwrap_or("")
            .to_owned();
        return RipgrepError::Usage { detail };
    }
    if stderr.contains("(os error 2)") || stderr.contains("No such file or directory") {
        return RipgrepError::NotFound;
    }
    if stderr.contains("(os error 13)") || stderr.contains("Permission denied") {
        return RipgrepError::Permission;
    }
    let detail = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned();
    RipgrepError::Runtime { detail }
}

/// Result of streaming and rendering rg's `--json` output.
struct GrepStreamResult {
    result_lines: Vec<String>,
    match_count: usize,
    lines_truncated: bool,
    match_limit_reached: bool,
}

/// Minimal rg `--json` envelope. Only the fields we render are
/// deserialized; everything else is dropped.
#[derive(serde::Deserialize)]
struct RgRecord {
    #[serde(rename = "type")]
    kind: String,
    data: RgData,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RgData {
    path: Option<RgText>,
    lines: Option<RgText>,
    line_number: Option<u64>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RgText {
    text: Option<String>,
    bytes: Option<String>,
}

impl RgText {
    fn render_path(&self) -> Option<String> {
        if let Some(text) = &self.text {
            return Some(escape_path_text(text));
        }
        self.decoded_bytes().map(|bytes| render_path_bytes(&bytes))
    }

    fn text_lossy(self) -> Option<String> {
        if let Some(text) = self.text {
            return Some(text);
        }
        self.decoded_bytes()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn decoded_bytes(&self) -> Option<Vec<u8>> {
        let bytes = self.bytes.as_ref()?;
        base64::Engine::decode(&path_base64_engine::general_purpose::STANDARD, bytes).ok()
    }
}

/// Stream rg's JSON Lines output into heading-grouped rendering, breaking
/// early once the match limit is reached. Each file's path is emitted once
/// as a heading line, followed by `LINE:CONTENT` match lines and
/// `LINE-CONTENT` context lines.
fn read_grep_json<R: Read>(stdout: R, limit: usize) -> GrepStreamResult {
    use std::io::BufRead as _;
    let reader = BufReader::new(stdout);
    let mut result_lines = Vec::new();
    let mut match_count = 0usize;
    let mut lines_truncated = false;
    let mut match_limit_reached = false;
    let mut current_path: Option<String> = None;
    let mut heading_path: Option<String> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<RgRecord>(&line) else {
            continue;
        };
        match record.kind.as_str() {
            "begin" => {
                current_path = record.data.path.as_ref().and_then(RgText::render_path);
            }
            "match" | "context" => {
                let path = record
                    .data
                    .path
                    .as_ref()
                    .and_then(RgText::render_path)
                    .or_else(|| current_path.clone())
                    .unwrap_or_default();
                let lineno = record.data.line_number.unwrap_or(0);
                let text = record
                    .data
                    .lines
                    .and_then(RgText::text_lossy)
                    .unwrap_or_default();
                let text = strip_eol(&text);
                let is_match = record.kind == "match";
                if is_match {
                    if limit <= match_count {
                        match_limit_reached = true;
                        break;
                    }
                    match_count += 1;
                }
                // Emit the file path once as a heading so match and context
                // lines don't repeat it on every record. This runs after the
                // limit check so a limit-triggered break leaves no dangling
                // heading with no body line beneath it.
                if heading_path.as_deref() != Some(path.as_str()) {
                    let (heading, heading_truncated) = render_grep_heading(&path);
                    if heading_truncated {
                        lines_truncated = true;
                    }
                    result_lines.push(heading);
                    heading_path = Some(path);
                }
                let sep = if is_match { ':' } else { '-' };
                let (rendered, truncated) = render_grep_line(lineno, sep, text);
                if truncated {
                    lines_truncated = true;
                }
                result_lines.push(rendered);
            }
            _ => {}
        }
    }

    GrepStreamResult {
        result_lines,
        match_count,
        lines_truncated,
        match_limit_reached,
    }
}

fn strip_eol(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

/// Build the CBOR result map for `grep` without echoing request arguments.
/// Call context such as `pattern`, `path`, and `glob` is already available to
/// callers from the tool invocation; repeating it in the result wastes tokens.
pub(crate) fn grep_result_map(
    status: Option<i32>,
    matches: usize,
    output_text: String,
) -> CborValue {
    CborValue::Map(vec![
        (
            CborValue::Text("status".to_owned()),
            status
                .map(|code| CborValue::Integer((code as i64).into()))
                .unwrap_or(CborValue::Null),
        ),
        (
            CborValue::Text("matches".to_owned()),
            CborValue::Integer((matches as i64).into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text(output_text.clone()),
        ),
        (
            CborValue::Text("output_lines".to_owned()),
            CborValue::Integer((output_text.lines().count() as i64).into()),
        ),
        (
            CborValue::Text("output_bytes".to_owned()),
            CborValue::Integer((output_text.len() as i64).into()),
        ),
    ])
}

/// Render a per-file path heading line, capping over-long paths at the
/// display budget with an ellipsis so every rendered line, heading included,
/// stays within `GREP_MAX_LINE_LENGTH`.
fn render_grep_heading(path: &str) -> (String, bool) {
    if path.len() <= GREP_MAX_LINE_LENGTH {
        return (path.to_owned(), false);
    }
    let ellipsis = "…";
    // `GREP_MAX_LINE_LENGTH` (500) far exceeds the ellipsis size, so the
    // content budget below is always positive.
    let mut end = (GREP_MAX_LINE_LENGTH - ellipsis.len()).min(path.len());
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{ellipsis}", &path[..end]), true)
}

/// Render a single match or context body line beneath a per-file heading, as
/// `LINE:CONTENT` for matches and `LINE-CONTENT` for context lines. Long
/// content is truncated at the display budget with an ellipsis.
fn render_grep_line(lineno: u64, sep: char, text: &str) -> (String, bool) {
    let prefix = format!("{lineno}{sep}");
    let rendered = format!("{prefix}{text}");
    if rendered.len() <= GREP_MAX_LINE_LENGTH {
        return (rendered, false);
    }

    let ellipsis = "…";
    // The prefix (`LINENO:` / `LINENO-`) is at most 21 bytes (20-digit u64
    // line number plus separator), so with the ellipsis it can never reach
    // `GREP_MAX_LINE_LENGTH` and the content budget below is always positive;
    // no `(truncated)` fallback is needed.
    let text_budget = GREP_MAX_LINE_LENGTH - prefix.len() - ellipsis.len();
    let mut end = text_budget.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{prefix}{}{}", &text[..end], ellipsis), true)
}

fn append_notices_within_cap(mut output_text: String, notices: &[String]) -> String {
    if notices.is_empty() {
        return output_text;
    }
    let notice = format!("\n\n[{}]", notices.join(" "));
    if output_text.len().saturating_add(notice.len()) <= MAX_OUTPUT_BYTES {
        output_text.push_str(&notice);
        return output_text;
    }
    let Some(budget) = MAX_OUTPUT_BYTES.checked_sub(notice.len()) else {
        return notice.chars().take(MAX_OUTPUT_BYTES).collect();
    };
    let mut end = budget.min(output_text.len());
    while !output_text.is_char_boundary(end) {
        end -= 1;
    }
    output_text.truncate(end);
    output_text.push_str(&notice);
    output_text
}

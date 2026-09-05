//! Resizable Unix PTY child with bounded VT capture and process-group cleanup.

#![cfg(unix)]

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{io as path_std_io, thread};

use nix::poll::{PollFd, PollFlags, poll};
use nix::pty::{Winsize, openpty};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use super::process_group;

const COLS: u16 = 120;
const ROWS: u16 = 40;
const MAX_RAW_BYTES: usize = 256 * 1024;
const MAX_FRAMES: usize = 512;

/// Named terminal cell dimensions shared by the kernel PTY and semantic VT.
pub(super) struct TerminalSize {
    /// Visible terminal columns.
    pub(super) cols: u16,
    /// Visible terminal rows.
    pub(super) rows: u16,
}

/// Monotonic count of PTY reads completed into the semantic VT model.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct PtyReadGeneration(
    /// Completed PTY reads; private so callers cannot forge a boundary.
    u64,
);

/// Semantic style attributes retained by the VT model for one cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct VtCellStyle {
    /// Resolved foreground color.
    foreground: vt100::Color,
    /// Resolved background color.
    background: vt100::Color,
    /// Whether the cell uses bold emphasis.
    bold: bool,
}

/// One atomic observation of a complete frame and its selected-agent styles.
pub(super) struct VtStyledFrame {
    /// Styles extracted from the exact frame below.
    pub(super) styles: Vec<VtCellStyle>,
    /// Normalized semantic VT frame that supplied `styles`.
    pub(super) frame: String,
}

/// Shared bounded terminal observations produced by the PTY reader.
struct Capture {
    /// Bounded raw PTY suffix for failure diagnostics.
    raw: Vec<u8>,
    /// Normalized semantic frames in observation order.
    frames: VecDeque<String>,
    /// Latest VT parser state.
    parser: vt100::Parser,
    /// Whether the PTY reader reached EOF.
    closed: bool,
    /// First forbidden historical pending/progress tool row, retained forever.
    tool_violation: Option<String>,
    /// Whether completed historical tool activity is forbidden for this boot.
    tool_latch_armed: bool,
    /// Number of completed PTY reads processed into the VT model.
    generation: PtyReadGeneration,
}

/// Test-only synchronization at one exact post-read/pre-processing boundary.
#[cfg(test)]
struct ReaderHook {
    /// One-based read number that must block.
    target_read: usize,
    /// Count of completed kernel reads.
    reads: AtomicUsize,
    /// Notification that target read bytes are present in memory.
    captured: mpsc::SyncSender<()>,
    /// Notification after one read's continuous artifacts are fully written.
    artifact_written: mpsc::SyncSender<usize>,
    /// Release sent after the test raises cooperative stop.
    release: Mutex<mpsc::Receiver<()>>,
}

/// One exact Tau process attached to a real pseudo-terminal.
pub(super) struct PtyProcess {
    /// Spawned Tau child, which is also the private process-group leader.
    child: Option<Child>,
    /// Writable clone of the PTY master.
    writer: Option<File>,
    /// Shared captured terminal state and wakeup generation.
    capture: Arc<(Mutex<Capture>, Condvar)>,
    /// Reader worker joined after the child process tree is gone.
    reader: Option<thread::JoinHandle<()>>,
    /// Bounded notification that the reader worker reached EOF.
    reader_done: mpsc::Receiver<()>,
    /// Cooperative bounded stop for the poll-based PTY reader.
    reader_stop: Arc<AtomicBool>,
    /// Bounded failure artifacts written only after the reader has stopped.
    artifacts: Option<PtyArtifacts>,
}

/// Named bounded diagnostic destinations for one PTY process.
pub(super) struct PtyArtifacts {
    /// Bounded raw PTY byte suffix.
    raw: PathBuf,
    /// Latest normalized VT frame.
    normalized: PathBuf,
}

impl PtyArtifacts {
    /// Creates destinations for raw and normalized PTY diagnostics.
    pub(super) fn new(raw: PathBuf, normalized: PathBuf) -> Self {
        Self { raw, normalized }
    }
}

impl PtyProcess {
    /// Resizes the kernel PTY and semantic VT while holding the capture lock.
    pub(super) fn resize(&mut self, size: TerminalSize) -> Result<(), Box<dyn std::error::Error>> {
        let writer = self.writer.as_ref().ok_or("PTY writer closed")?;
        let winsize = Winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let mut capture = self.capture.0.lock().map_err(|_| "PTY capture poisoned")?;
        // SAFETY: `writer` is the owned PTY master and `winsize` outlives ioctl.
        #[allow(unsafe_code)]
        if unsafe { nix::libc::ioctl(writer.as_fd().as_raw_fd(), nix::libc::TIOCSWINSZ, &winsize) }
            == -1
        {
            return Err(path_std_io::Error::last_os_error().into());
        }
        capture.parser.screen_mut().set_size(size.rows, size.cols);
        Ok(())
    }

    /// Spawns `command` in a fresh session with fixed initial dimensions.
    pub(super) fn spawn(
        mut command: Command,
        tool_latch_armed: bool,
        artifacts: Option<PtyArtifacts>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let pty = openpty(
            Some(&Winsize {
                ws_row: ROWS,
                ws_col: COLS,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )?;
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);
        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: `setsid` and `ioctl(TIOCSCTTY)` are async-signal-safe, use
        // only the already-installed PTY stdin descriptor, and run before exec.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || {
                if nix::libc::setsid() == -1 {
                    return Err(path_std_io::Error::last_os_error());
                }
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) == -1 {
                    return Err(path_std_io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let writer = master.try_clone()?;
        let capture = Arc::new((
            Mutex::new(Capture {
                raw: Vec::new(),
                frames: VecDeque::new(),
                parser: vt100::Parser::new(ROWS, COLS, 2_000),
                closed: false,
                tool_violation: None,
                tool_latch_armed,
                generation: PtyReadGeneration(0),
            }),
            Condvar::new(),
        ));
        let reader_capture = Arc::clone(&capture);
        let (reader_finished, reader_done) = mpsc::channel();
        let reader_stop = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&reader_stop);
        let reader = thread::spawn(move || {
            read_pty(master, &reader_capture, &stop, None, None);
            let _ = reader_finished.send(());
        });
        Ok(Self {
            child: Some(child),
            writer: Some(writer),
            capture,
            reader: Some(reader),
            reader_done,
            reader_stop,
            artifacts,
        })
    }

    /// Sends one submitted terminal line.
    pub(super) fn send_line(&mut self, line: &str) -> Result<(), std::io::Error> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| path_std_io::Error::other("PTY writer closed"))?;
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\r")?;
        writer.flush()
    }

    /// Sends editable prompt text without submitting it.
    pub(super) fn send_text(&mut self, text: &str) -> Result<(), std::io::Error> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| path_std_io::Error::other("PTY writer closed"))?;
        writer.write_all(text.as_bytes())?;
        writer.flush()
    }

    /// Sends Ctrl-U to clear the current editable prompt without submission.
    pub(super) fn send_clear_prompt_key(&mut self) -> Result<(), std::io::Error> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| path_std_io::Error::other("PTY writer closed"))?;
        writer.write_all(b"\x15")?;
        writer.flush()
    }

    /// Deletes `count` editable characters without submitting the prompt.
    pub(super) fn send_backspaces(&mut self, count: usize) -> Result<(), std::io::Error> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| path_std_io::Error::other("PTY writer closed"))?;
        writer.write_all(&vec![0x7f; count])?;
        writer.flush()
    }

    /// Sends the ordinary Ctrl-J next-agent navigation key.
    pub(super) fn send_next_agent_key(&mut self) -> Result<(), std::io::Error> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| path_std_io::Error::other("PTY writer closed"))?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    /// Waits until the normalized current screen contains `needle`.
    pub(super) fn wait_for(
        &self,
        needle: &str,
        deadline: Instant,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let (lock, wake) = &*self.capture;
        let mut capture = lock.lock().map_err(|_| "PTY capture poisoned")?;
        loop {
            let frame = normalized_screen(&capture.parser);
            if frame.contains(needle) {
                return Ok(frame);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || capture.closed {
                return Err(format!(
                    "timed out waiting for terminal `{needle}`; last frame:\n{frame}"
                )
                .into());
            }
            let (next, _) = wake
                .wait_timeout(capture, remaining.min(Duration::from_millis(100)))
                .map_err(|_| "PTY capture poisoned")?;
            capture = next;
        }
    }

    /// Returns the completed-read generation of the semantic VT model.
    pub(super) fn read_generation(&self) -> Result<PtyReadGeneration, Box<dyn std::error::Error>> {
        Ok(self
            .capture
            .0
            .lock()
            .map_err(|_| "PTY capture poisoned")?
            .generation)
    }

    /// Waits until one marker is absent from a strictly newer VT generation.
    pub(super) fn wait_for_absence_after(
        &self,
        marker: &str,
        generation: PtyReadGeneration,
        deadline: Instant,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let (lock, wake) = &*self.capture;
        let mut capture = lock.lock().map_err(|_| "PTY capture poisoned")?;
        loop {
            let frame = normalized_screen(&capture.parser);
            if generation < capture.generation && !frame.contains(marker) {
                return Ok(frame);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || capture.closed {
                return Err(format!(
                    "timed out waiting for `{marker}` to clear; last frame:\n{frame}"
                )
                .into());
            }
            let (next, _) = wake
                .wait_timeout(capture, remaining)
                .map_err(|_| "PTY capture poisoned")?;
            capture = next;
        }
    }

    /// Waits for a newer generation satisfying one complete semantic predicate.
    pub(super) fn wait_for_frame_after(
        &self,
        generation: PtyReadGeneration,
        deadline: Instant,
        is_complete: impl Fn(&str) -> bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        wait_for_complete_frame_after(&self.capture, generation, deadline, is_complete)
    }

    /// Waits for one newer complete frame and extracts its selected-row styles
    /// from the same locked VT observation.
    pub(super) fn wait_for_styled_frame_after(
        &self,
        generation: PtyReadGeneration,
        marker: &str,
        deadline: Instant,
        is_complete: impl Fn(&str) -> bool,
    ) -> Result<VtStyledFrame, Box<dyn std::error::Error>> {
        wait_for_complete_styled_frame_after(
            &self.capture,
            generation,
            marker,
            deadline,
            is_complete,
        )
    }

    /// Waits until an exact visible marker's VT styles differ from `previous`.
    pub(super) fn wait_for_marker_style_change(
        &self,
        marker: &str,
        previous: &[VtCellStyle],
        deadline: Instant,
    ) -> Result<VtStyledFrame, Box<dyn std::error::Error>> {
        if !marker.is_ascii() {
            return Err("selected-agent style marker must be ASCII".into());
        }
        let (lock, wake) = &*self.capture;
        let mut capture = lock.lock().map_err(|_| "PTY capture poisoned")?;
        loop {
            if let Ok(styles) = selected_agent_status_styles(&capture.parser, marker)
                && styles != previous
            {
                return Ok(VtStyledFrame {
                    styles,
                    frame: normalized_screen(&capture.parser),
                });
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || capture.closed {
                return Err(format!(
                    "timed out waiting for selected-agent style change; last frame:\n{}",
                    normalized_screen(&capture.parser)
                )
                .into());
            }
            let (next, _) = wake
                .wait_timeout(capture, remaining)
                .map_err(|_| "PTY capture poisoned")?;
            capture = next;
        }
    }

    /// Waits for the real empty editable prompt with the VT cursor on its row.
    pub(super) fn wait_ready(
        &self,
        deadline: Instant,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.wait_for_prompt("editable terminal prompt", deadline, prompt_ready)
    }

    /// Waits for the fresh-session composer naming the exact role used to start
    /// the first agent.
    pub(super) fn wait_ready_to_start_role(
        &mut self,
        role: &str,
        deadline: Instant,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let needle = format!("Write a message to start a new {role} agent...");
        self.wait_for_prompt(&format!("prompt `{needle}`"), deadline, |parser| {
            prompt_ready_for(parser, &needle)
        })
    }

    /// Waits for an empty editable prompt targeting one exact selected agent.
    pub(super) fn wait_ready_for(
        &self,
        agent_id: &str,
        deadline: Instant,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let needle = format!("Write a message to {agent_id}...");
        self.wait_for_prompt(&format!("prompt `{needle}`"), deadline, |parser| {
            prompt_ready_for(parser, &needle)
        })
    }

    fn wait_for_prompt(
        &self,
        description: &str,
        deadline: Instant,
        is_ready: impl Fn(&vt100::Parser) -> bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let (lock, wake) = &*self.capture;
        let mut capture = lock.lock().map_err(|_| "PTY capture poisoned")?;
        loop {
            let frame = normalized_screen(&capture.parser);
            if is_ready(&capture.parser) {
                return Ok(frame);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || capture.closed {
                return Err(
                    format!("timed out waiting for {description}; last frame:\n{frame}").into(),
                );
            }
            let (next, _) = wake
                .wait_timeout(capture, remaining)
                .map_err(|_| "PTY capture poisoned")?;
            capture = next;
        }
    }

    /// Fails if any byte-wise VT state showed a completed historical tool row
    /// as pending or progress, even when later output replaced it in the
    /// same read.
    pub(super) fn require_no_tool_violation(&self) -> Result<(), Box<dyn std::error::Error>> {
        let capture = self.capture.0.lock().map_err(|_| "PTY capture poisoned")?;
        if let Some(row) = &capture.tool_violation {
            return Err(format!("completed tool transiently became active: {row}").into());
        }
        Ok(())
    }

    /// Arms sticky monitoring after the restored terminal row is established.
    pub(super) fn start_tool_monitoring(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut capture = self.capture.0.lock().map_err(|_| "PTY capture poisoned")?;
        capture.tool_violation = None;
        capture.tool_latch_armed = true;
        Ok(())
    }

    /// Atomically verifies and disarms the Boot-B historical-tool monitor after
    /// the fresh turn has reached terminal VT readiness.
    pub(super) fn finish_tool_monitoring(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut capture = self.capture.0.lock().map_err(|_| "PTY capture poisoned")?;
        if let Some(row) = &capture.tool_violation {
            return Err(format!("completed tool transiently became active: {row}").into());
        }
        capture.tool_latch_armed = false;
        Ok(())
    }

    /// Returns the bounded raw terminal suffix for retained diagnostics.
    pub(super) fn raw(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        Ok(self
            .capture
            .0
            .lock()
            .map_err(|_| "PTY capture poisoned")?
            .raw
            .clone())
    }

    /// Returns the number of terminal bells retained in the bounded raw suffix.
    pub(super) fn retained_bell_count(&self) -> Result<usize, Box<dyn std::error::Error>> {
        Ok(self.raw()?.iter().filter(|byte| **byte == b'\x07').count())
    }

    /// Waits until a live terminal bell arrives after the caller's baseline.
    pub(super) fn wait_for_bell_after(
        &self,
        baseline: usize,
        deadline: Instant,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (lock, wake) = &*self.capture;
        let mut capture = lock.lock().map_err(|_| "PTY capture poisoned")?;
        loop {
            if capture.raw.iter().filter(|byte| **byte == b'\x07').count() > baseline {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || capture.closed {
                return Err("timed out waiting for terminal bell fence".into());
            }
            let (next, _) = wake
                .wait_timeout(capture, remaining.min(Duration::from_millis(100)))
                .map_err(|_| "PTY capture poisoned")?;
            capture = next;
        }
    }

    /// Requests explicit session shutdown, then waits for the owned process
    /// tree's natural exit.
    pub(super) fn finish(mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.send_line(":quit-session")?;
        self.reap_naturally()
    }

    /// Reaps a process expected to exit naturally and returns bytes after
    /// reader drain.
    pub(super) fn finish_exited(mut self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        self.reap_naturally()?;
        self.raw()
    }

    /// Waits for the explicitly requested exit, natural PTY EOF, and complete
    /// process-group teardown without racing a fixture-local wall-clock
    /// deadline.
    fn reap_naturally(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let child = self.child.as_mut().ok_or("PTY child already reaped")?;
        let pgid = Pid::from_raw(child.id() as i32);
        let status = child.wait()?;
        if !status.success() {
            return Err(format!("Tau PTY process exited with {status}").into());
        }
        self.writer.take();
        self.reader_done
            .recv()
            .map_err(|_| "PTY reader stopped without natural EOF")?;
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| "PTY reader panicked")?;
        }
        while process_group::exists(pgid) {
            thread::yield_now();
        }
        self.write_artifacts()?;
        self.child.take();
        Ok(())
    }

    /// Reaps the child, escalating to process-group termination on deadline.
    fn reap(
        &mut self,
        graceful: Duration,
    ) -> Result<std::process::ExitStatus, Box<dyn std::error::Error>> {
        self.reader_stop.store(true, Ordering::Release);
        let child = self.child.as_mut().ok_or("PTY child already reaped")?;
        let pgid = Pid::from_raw(child.id() as i32);
        let first_deadline = Instant::now() + graceful;
        let mut status = None;
        while Instant::now() < first_deadline {
            status = status.or(child.try_wait()?);
            if status.is_some() && !process_group::exists(pgid) {
                break;
            }
            thread::yield_now();
        }
        if process_group::exists(pgid) {
            let _ = killpg(pgid, Signal::SIGTERM);
            let term_deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < term_deadline && process_group::exists(pgid) {
                status = status.or(child.try_wait()?);
                thread::yield_now();
            }
        }
        if process_group::exists(pgid) {
            let _ = killpg(pgid, Signal::SIGKILL);
            let kill_deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < kill_deadline && process_group::exists(pgid) {
                status = status.or(child.try_wait()?);
                thread::yield_now();
            }
        }
        if process_group::exists(pgid) {
            return Err("PTY process group survived SIGKILL deadline".into());
        }
        let parent_deadline = Instant::now() + Duration::from_secs(1);
        while status.is_none() && Instant::now() < parent_deadline {
            status = child.try_wait()?;
            thread::yield_now();
        }
        let status = status.ok_or("PTY parent survived process-group cleanup")?;
        self.writer.take();
        if self
            .reader_done
            .recv_timeout(Duration::from_secs(1))
            .is_err()
        {
            return Err("PTY reader did not close within cleanup deadline".into());
        }
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| "PTY reader panicked")?;
        }
        self.write_artifacts()?;
        self.child.take();
        Ok(status)
    }

    fn write_artifacts(&self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(artifacts) = &self.artifacts else {
            return Ok(());
        };
        let capture = self.capture.0.lock().map_err(|_| "PTY capture poisoned")?;
        std::fs::write(&artifacts.raw, &capture.raw)?;
        std::fs::write(&artifacts.normalized, normalized_screen(&capture.parser))?;
        Ok(())
    }
}

fn wait_for_complete_frame_after(
    capture: &Arc<(Mutex<Capture>, Condvar)>,
    generation: PtyReadGeneration,
    deadline: Instant,
    is_complete: impl Fn(&str) -> bool,
) -> Result<String, Box<dyn std::error::Error>> {
    let (lock, wake) = &**capture;
    let mut capture = lock.lock().map_err(|_| "PTY capture poisoned")?;
    loop {
        let frame = normalized_screen(&capture.parser);
        if generation < capture.generation && is_complete(&frame) {
            return Ok(frame);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || capture.closed {
            return Err(format!(
                "timed out waiting for complete newer frame; last frame:\n{frame}"
            )
            .into());
        }
        let (next, _) = wake
            .wait_timeout(capture, remaining)
            .map_err(|_| "PTY capture poisoned")?;
        capture = next;
    }
}

fn wait_for_complete_styled_frame_after(
    capture: &Arc<(Mutex<Capture>, Condvar)>,
    generation: PtyReadGeneration,
    marker: &str,
    deadline: Instant,
    is_complete: impl Fn(&str) -> bool,
) -> Result<VtStyledFrame, Box<dyn std::error::Error>> {
    if !marker.is_ascii() {
        return Err("selected-agent style marker must be ASCII".into());
    }
    let (lock, wake) = &**capture;
    let mut capture = lock.lock().map_err(|_| "PTY capture poisoned")?;
    loop {
        let frame = normalized_screen(&capture.parser);
        if generation < capture.generation
            && is_complete(&frame)
            && let Ok(styles) = selected_agent_status_styles(&capture.parser, marker)
        {
            return Ok(VtStyledFrame { styles, frame });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || capture.closed {
            return Err(format!(
                "timed out waiting for complete newer styled frame; last frame:\n{frame}"
            )
            .into());
        }
        let (next, _) = wake
            .wait_timeout(capture, remaining)
            .map_err(|_| "PTY capture poisoned")?;
        capture = next;
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        let _ = self.reap(Duration::ZERO);
        let _ = self.write_artifacts();
    }
}

fn read_pty(
    mut master: File,
    capture: &Arc<(Mutex<Capture>, Condvar)>,
    stop: &AtomicBool,
    artifacts: Option<&(PathBuf, PathBuf)>,
    #[cfg(test)] hook: Option<&ReaderHook>,
) {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let ready = {
            let mut descriptors = [PollFd::new(
                master.as_fd(),
                PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
            )];
            match poll(&mut descriptors, 100_u16) {
                Ok(0) => continue,
                Ok(_) => descriptors[0].revents().unwrap_or_else(PollFlags::empty),
                Err(_) => break,
            }
        };
        if ready.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            break;
        }
        match master.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let (lock, wake) = &**capture;
                let Ok(mut capture) = lock.lock() else {
                    return;
                };
                capture.raw.extend_from_slice(&buffer[..read]);
                if capture.raw.len() > MAX_RAW_BYTES {
                    let excess = capture.raw.len() - MAX_RAW_BYTES;
                    capture.raw.drain(..excess);
                }
                #[cfg(test)]
                let read_number = hook.map(|hook| hook.reads.fetch_add(1, Ordering::AcqRel) + 1);
                #[cfg(test)]
                if let Some(hook) = hook
                    && read_number == Some(hook.target_read)
                {
                    let _ = hook.captured.send(());
                    let _ = hook
                        .release
                        .lock()
                        .map_err(|_| ())
                        .and_then(|release| release.recv().map_err(|_| ()));
                }
                let complete = process_capture_bytes(&mut capture, &buffer[..read], |_: usize| {
                    stop.load(Ordering::Acquire)
                });
                if !complete {
                    wake.notify_all();
                    break;
                }
                capture.generation.0 = capture.generation.0.saturating_add(1);
                let frame = normalized_screen(&capture.parser);
                if capture.frames.back() != Some(&frame) {
                    capture.frames.push_back(frame.clone());
                    if capture.frames.len() > MAX_FRAMES {
                        capture.frames.pop_front();
                    }
                }
                if let Some((raw_path, frame_path)) = artifacts {
                    let _ = std::fs::write(raw_path, &capture.raw);
                    let _ = std::fs::write(frame_path, frame.as_bytes());
                }
                #[cfg(test)]
                if let (Some(hook), Some(read_number)) = (hook, read_number) {
                    let _ = hook.artifact_written.send(read_number);
                }
                wake.notify_all();
            }
        }
    }
    let (lock, wake) = &**capture;
    if let Ok(mut capture) = lock.lock() {
        capture.closed = true;
        wake.notify_all();
    }
}

fn process_capture_bytes(
    capture: &mut Capture,
    bytes: &[u8],
    mut should_stop: impl FnMut(usize) -> bool,
) -> bool {
    for (index, byte) in bytes.iter().enumerate() {
        if should_stop(index) {
            return false;
        }
        if byte.is_ascii_control() {
            latch_tool_violation(capture);
        }
        capture.parser.process(std::slice::from_ref(byte));
        if index % 64 == 63 || index + 1 == bytes.len() || matches!(*byte, b'\r' | b'\n') {
            latch_tool_violation(capture);
        }
    }
    true
}

fn latch_tool_violation(capture: &mut Capture) {
    if capture.tool_latch_armed
        && capture.tool_violation.is_none()
        && let Some(row) = normalized_screen(&capture.parser)
            .lines()
            .find(|line| line.contains("restart_test_dummy") || line.contains("agent_start"))
        && (row.contains("pending") || row.contains('…'))
    {
        capture.tool_violation = Some(row.to_owned());
    }
}

fn normalized_screen(parser: &vt100::Parser) -> String {
    parser
        .screen()
        .contents()
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extracts styles from one exact ASCII selected-agent status row.
fn selected_agent_status_styles(
    parser: &vt100::Parser,
    marker: &str,
) -> Result<Vec<VtCellStyle>, Box<dyn std::error::Error>> {
    if !marker.is_ascii() {
        return Err("selected-agent style marker must be ASCII".into());
    }
    let contents = parser.screen().contents();
    let expected_identity = format!("@{marker} ");
    let matches = contents
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(&expected_identity))
        .collect::<Vec<_>>();
    let [(row, _)] = matches.as_slice() else {
        return Err(format!(
            "expected one selected-agent status row for `{marker}`, found {}",
            matches.len()
        )
        .into());
    };
    let col = (0..parser.screen().size().1)
        .find(|&column| {
            marker.bytes().enumerate().all(|(offset, byte)| {
                parser
                    .screen()
                    .cell(*row as u16, column + offset as u16)
                    .is_some_and(|cell| cell.contents() == char::from(byte).to_string())
            })
        })
        .ok_or("selected-agent marker missing from matched status row")?;
    marker
        .bytes()
        .enumerate()
        .map(|(offset, _)| {
            let cell = parser
                .screen()
                .cell(*row as u16, col + offset as u16)
                .ok_or("selected-agent marker cell is outside the visible VT")?;
            Ok(VtCellStyle {
                foreground: cell.fgcolor(),
                background: cell.bgcolor(),
                bold: cell.bold(),
            })
        })
        .collect()
}

fn prompt_ready(parser: &vt100::Parser) -> bool {
    prompt_ready_for(parser, "Write a message to main...")
}

fn prompt_ready_for(parser: &vt100::Parser, needle: &str) -> bool {
    let (cursor_row, _) = parser.screen().cursor_position();
    parser
        .screen()
        .contents()
        .lines()
        .enumerate()
        .any(|(row, line)| {
            row as u16 == cursor_row && line.contains(needle) && !line.contains("pending")
        })
}

/// Ensures the style oracle selects one anchored status row rather than an
/// earlier transcript occurrence of the same ASCII agent id.
#[test]
fn selected_agent_status_styles_require_one_anchored_row() {
    let mut parser = vt100::Parser::new(4, 80, 0);
    parser.process(b"message mentions main\r\n\x1b[1;35;44m@main status\x1b[0m");
    let styles = selected_agent_status_styles(&parser, "main").expect("unique status row");
    assert_eq!(
        styles,
        vec![
            VtCellStyle {
                foreground: vt100::Color::Idx(5),
                background: vt100::Color::Idx(4),
                bold: true,
            };
            4
        ]
    );
    assert!(selected_agent_status_styles(&parser, "maïn").is_err());

    parser.process(b"\r\n@main duplicate");
    assert!(selected_agent_status_styles(&parser, "main").is_err());
}

/// Ensures the sticky oracle catches a pending-to-ok repaint even when both
/// frames arrive in one kernel read.
#[test]
fn bytewise_capture_latches_pending_before_same_read_terminal_repaint() {
    let mut capture = Capture {
        raw: Vec::new(),
        frames: VecDeque::new(),
        parser: vt100::Parser::new(ROWS, COLS, 10),
        closed: false,
        tool_violation: None,
        tool_latch_armed: true,
        generation: PtyReadGeneration(0),
    };
    process_capture_bytes(
        &mut capture,
        b"restart_test_dummy pending\r\x1b[2Krestart_test_dummy ok",
        |_| false,
    );
    assert!(capture.tool_violation.is_some());
    assert!(normalized_screen(&capture.parser).contains("restart_test_dummy ok"));
}

/// Ensures S8 applies the same sticky historical-row oracle to the production
/// `agent_start` call that created the restored worker.
#[test]
fn bytewise_capture_latches_agent_start_pending_repaint() {
    let mut capture = Capture {
        raw: Vec::new(),
        frames: VecDeque::new(),
        parser: vt100::Parser::new(ROWS, COLS, 10),
        closed: false,
        tool_violation: None,
        tool_latch_armed: true,
        generation: PtyReadGeneration(0),
    };
    process_capture_bytes(
        &mut capture,
        b"agent_start [worker] pending\r\x1b[2Kagent_start [worker] ok",
        |_| false,
    );
    assert!(capture.tool_violation.is_some());
    assert!(normalized_screen(&capture.parser).contains("agent_start [worker] ok"));
}

/// Ensures cursor-repositioning C0 controls cannot erase a pending state before
/// the sticky VT oracle observes it.
#[test]
fn bytewise_capture_latches_pending_before_backspace_overwrite() {
    let mut capture = Capture {
        raw: Vec::new(),
        frames: VecDeque::new(),
        parser: vt100::Parser::new(ROWS, COLS, 10),
        closed: false,
        tool_violation: None,
        tool_latch_armed: true,
        generation: PtyReadGeneration(0),
    };
    process_capture_bytes(
        &mut capture,
        b"restart_test_dummy pending\x08\x08\x08\x08\x08\x08\x08ok     ",
        |_| false,
    );
    assert!(capture.tool_violation.is_some());
}

/// Ensures complete styled-frame waiting rejects an idle-only redraw before
/// atomically accepting the subsequent selected-agent status row.
#[test]
fn complete_frame_wait_rejects_idle_only_generation() {
    let mut command = Command::new("sh");
    command.arg("-c").arg(
        "read first; \
         printf 'This active-auto agent is idle phase-one'; \
         read second; \
         printf ' phase-two'; \
         read third; \
         printf '\\r\\n@worker $.00/$.00'; \
         read fourth",
    );
    let mut process = PtyProcess::spawn(command, false, None).expect("spawn fragmented redraw");
    let mut writer = process
        .writer
        .as_ref()
        .expect("PTY writer")
        .try_clone()
        .expect("clone PTY writer");
    let before_idle = process.read_generation().expect("read initial generation");
    let capture = Arc::clone(&process.capture);
    let (result_tx, result_rx) = mpsc::channel();
    let (evaluated_tx, evaluated_rx) = mpsc::channel();

    thread::scope(|scope| {
        scope.spawn(|| {
            let result = wait_for_complete_styled_frame_after(
                &capture,
                before_idle,
                "worker",
                Instant::now() + Duration::from_secs(1),
                |frame| {
                    if frame.contains("This active-auto agent is idle") {
                        let phase = u8::from(frame.contains("phase-two")) + 1;
                        evaluated_tx.send(phase).expect("report evaluated phase");
                        true
                    } else {
                        false
                    }
                },
            )
            .map_err(|error| error.to_string());
            result_tx.send(result).expect("return complete frame");
        });
        writer.write_all(b"first\r").expect("release idle redraw");
        writer.flush().expect("flush idle redraw release");
        process
            .wait_for("phase-one", Instant::now() + Duration::from_secs(1))
            .expect("observe idle-only redraw");
        while evaluated_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiter evaluated phase one")
            != 1
        {}
        writer
            .write_all(b"second\r")
            .expect("release second idle-only redraw");
        writer
            .flush()
            .expect("flush second idle-only redraw release");
        while evaluated_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiter evaluated phase two")
            != 2
        {}
        assert!(matches!(
            result_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        writer
            .write_all(b"third\r")
            .expect("release selected status redraw");
        writer
            .flush()
            .expect("flush selected status redraw release");
        let observation = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("complete frame result")
            .expect("complete selected status frame");
        assert!(observation.frame.contains("This active-auto agent is idle"));
        assert!(
            observation
                .frame
                .lines()
                .any(|line| line.contains("@worker "))
        );
        assert_eq!(observation.styles.len(), "worker".len());
        writer
            .write_all(b"fourth\r")
            .expect("release selected status process");
        writer
            .flush()
            .expect("flush selected status process release");
    });
    process
        .reap(Duration::from_secs(1))
        .expect("reap redraw process");
}

/// Ensures an armed near-max chunk honors a cooperative mid-chunk stop while
/// retaining the already-observed violation and bounded raw diagnostic.
#[test]
fn armed_capture_stops_mid_chunk_without_losing_prior_violation() {
    let mut capture = Capture {
        raw: Vec::new(),
        frames: VecDeque::new(),
        parser: vt100::Parser::new(ROWS, COLS, 10),
        closed: false,
        tool_violation: None,
        tool_latch_armed: true,
        generation: PtyReadGeneration(0),
    };
    let mut bytes = b"restart_test_dummy pending\r".to_vec();
    bytes.resize(MAX_RAW_BYTES, b'x');
    capture.raw.extend_from_slice(&bytes);
    let stop_at = b"restart_test_dummy pending\r".len() + 128;
    let started = Instant::now();
    let complete = process_capture_bytes(&mut capture, &bytes, |index| stop_at <= index);
    assert!(!complete);
    assert!(capture.tool_violation.is_some());
    assert_eq!(capture.raw.len(), MAX_RAW_BYTES);
    assert!(started.elapsed() < Duration::from_secs(1));
}

/// Ensures the real reader thread acknowledges stop within bounds without
/// rewriting the last valid continuous artifact after a large queued chunk.
#[test]
fn reader_thread_stop_preserves_last_pre_stop_artifact() {
    let pty = openpty(
        Some(&Winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None,
    )
    .expect("open pty");
    let master = File::from(pty.master);
    let mut slave = File::from(pty.slave);
    let capture = Arc::new((
        Mutex::new(Capture {
            raw: Vec::new(),
            frames: VecDeque::new(),
            parser: vt100::Parser::new(ROWS, COLS, 10),
            closed: false,
            tool_violation: None,
            tool_latch_armed: true,
            generation: PtyReadGeneration(0),
        }),
        Condvar::new(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    let raw_path = tempdir.path().join("raw");
    let frame_path = tempdir.path().join("frame");
    let artifacts = (raw_path.clone(), frame_path.clone());
    let reader_capture = Arc::clone(&capture);
    let reader_stop = Arc::clone(&stop);
    let (done_tx, done_rx) = mpsc::channel();
    let (captured_tx, captured_rx) = mpsc::sync_channel(0);
    let (artifact_tx, artifact_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let hook = Arc::new(ReaderHook {
        target_read: 2,
        reads: AtomicUsize::new(0),
        captured: captured_tx,
        artifact_written: artifact_tx,
        release: Mutex::new(release_rx),
    });
    let reader_hook = Arc::clone(&hook);
    let reader = thread::spawn(move || {
        read_pty(
            master,
            &reader_capture,
            &reader_stop,
            Some(&artifacts),
            Some(&reader_hook),
        );
        let _ = done_tx.send(());
    });

    slave.write_all(b"last-valid-frame").expect("seed output");
    slave.flush().expect("flush seed");
    assert_eq!(
        artifact_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("seed artifact completion"),
        1
    );
    let raw_before = std::fs::read(&raw_path).expect("pre-stop raw artifact");
    let frame_before = std::fs::read(&frame_path).expect("pre-stop frame artifact");
    slave
        .write_all(&vec![b'x'; 8 * 1024])
        .expect("queue large chunk");
    captured_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("reader captured queued chunk");
    stop.store(true, Ordering::Release);
    release_tx.send(()).expect("release reader");
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("bounded reader acknowledgement");
    reader.join().expect("reader join");
    let captured = capture.0.lock().expect("capture");
    assert!(captured.raw.len() > raw_before.len());
    assert!(captured.raw.contains(&b'x'));
    assert_eq!(std::fs::read(&raw_path).expect("raw artifact"), raw_before);
    assert_eq!(
        std::fs::read(&frame_path).expect("frame artifact"),
        frame_before
    );
}

/// Ensures cleanup does not stop at a successfully exited leader and escalates
/// against a surviving same-session descendant that ignores SIGTERM.
#[test]
fn cleanup_reaps_descendant_after_process_group_leader_exits() {
    let mut command = Command::new("sh");
    command.arg("-c").arg(
        "trap '' HUP TERM; \
         (trap '' HUP TERM; printf descendant-ready; while :; do :; done) & \
         exit 0",
    );
    let mut process =
        PtyProcess::spawn(command, false, None).expect("spawn adversarial process group");
    process
        .wait_for("descendant-ready", Instant::now() + Duration::from_secs(1))
        .expect("descendant readiness");
    let started = Instant::now();
    let status = process
        .reap(Duration::from_millis(20))
        .expect("bounded group cleanup");
    assert!(status.success());
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(started.elapsed() < Duration::from_secs(3));
}

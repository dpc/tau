//! Bounded byte capture and 15-second idle / 30-second pending-batch cadence.

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
#[cfg(unix)]
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Wake, Waker};
use std::time::{Duration, Instant};

use tau_proto::{Event, HarnessInputMessage, ToolOutputReport};

use super::OutputStream;
use crate::Output;
use crate::tools::world::RecordedShellUpdate;

/// Optional VCR copy of the same bounded batches, never an aggregate capture.
pub(super) type OutputRecording = Arc<Mutex<Vec<RecordedShellUpdate>>>;

/// Coalesced capacity readiness independent of pipe activity.
struct CapacityWake {
    /// Sticky retry permission consumed by the command loop.
    ready: AtomicBool,
    /// Platform event-loop wake, installed before any admission attempt.
    notify: Mutex<Option<Waker>>,
}

impl Wake for CapacityWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.ready.swap(true, Ordering::AcqRel) {
            let notify = self.notify.lock().expect("capacity notifier").clone();
            if let Some(notify) = notify {
                notify.wake();
            }
        }
    }
}

/// Owned nonblocking capacity pipe; retaining both ends prevents late SIGPIPE.
#[cfg(unix)]
struct CapacityPipe {
    /// Wake endpoints shared with the writer's callback.
    pipe: super::ShellWaitWakePipe,
}

#[cfg(unix)]
impl Wake for CapacityPipe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        super::shell_wait_write_wake_byte(&self.pipe.write);
    }
}

/// One run of bytes from a single pipe. Labels remain attached when older
/// fragments are dropped, so a retained tail never loses its stream identity.
struct Fragment {
    /// Whether these bytes originated on stderr rather than stdout.
    stderr: bool,
    /// Normalized UTF-8 bytes retained for this pipe fragment.
    text: String,
}

impl Fragment {
    fn label(&self) -> &'static str {
        if self.stderr {
            "\nerr (fragment) "
        } else {
            "\nout (fragment) "
        }
    }

    fn rendered_bytes(&self) -> usize {
        self.label().len() + self.text.len()
    }
}

/// Rolling recent output, bounded in both text and fragment count by its byte
/// allowance. Omission counts describe the normalized rendered representation.
#[derive(Default)]
struct PendingOutput {
    /// Recent fragments in pipe-observation order.
    fragments: VecDeque<Fragment>,
    /// Rendered bytes including stream labels.
    bytes: usize,
    /// Rendered bytes discarded to preserve the bounded recent tail.
    omitted: u64,
}

impl PendingOutput {
    fn push(&mut self, stderr: bool, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self
            .fragments
            .back_mut()
            .filter(|last| last.stderr == stderr)
        {
            last.text.push_str(text);
            self.bytes += text.len();
        } else {
            let fragment = Fragment {
                stderr,
                text: text.to_owned(),
            };
            self.bytes += fragment.rendered_bytes();
            self.fragments.push_back(fragment);
        }
        self.trim(tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES);
    }

    fn trim(&mut self, limit: usize) {
        while self.bytes > limit {
            let first = self
                .fragments
                .front_mut()
                .expect("nonempty bounded fragments");
            let excess = self.bytes - limit;
            if excess >= first.text.len() {
                let removed = first.rendered_bytes();
                self.fragments.pop_front();
                self.bytes -= removed;
                self.omitted = self.omitted.saturating_add(removed as u64);
            } else {
                let mut end = excess;
                while !first.text.is_char_boundary(end) {
                    end += 1;
                }
                first.text.drain(..end);
                self.bytes -= end;
                self.omitted = self.omitted.saturating_add(end as u64);
                if first.text.is_empty() {
                    let label_bytes = first.label().len();
                    self.fragments.pop_front();
                    self.bytes -= label_bytes;
                    self.omitted = self.omitted.saturating_add(label_bytes as u64);
                }
            }
        }
    }

    fn take(&mut self, limit: usize) -> (String, u64) {
        self.trim(limit);
        let mut text = String::with_capacity(self.bytes);
        for fragment in self.fragments.drain(..) {
            text.push_str(fragment.label());
            text.push_str(&fragment.text);
        }
        self.bytes = 0;
        (text, std::mem::take(&mut self.omitted))
    }
}

/// Per-invocation capture, independent of the aggregate/spool renderer.
pub(crate) struct OutputStreamCapture {
    /// Ordered publisher with bounded detached admission.
    output: Output,
    /// Immutable invocation identity echoed on every update.
    call_id: tau_proto::ToolCallId,
    /// Producer's registered local tool name.
    tool_name: tau_proto::ToolName,
    /// Next immutable report sequence.
    sequence: u64,
    /// Decoded pending text; never exceeds the per-frame allowance.
    pending: PendingOutput,
    /// First byte of the current pending batch.
    first: Option<Instant>,
    /// Most recent nonempty read, including omitted bytes.
    last: Option<Instant>,
    /// Incomplete stdout codepoint, at most three bytes.
    stdout_carry: Vec<u8>,
    /// Incomplete stderr codepoint, at most three bytes.
    stderr_carry: Vec<u8>,
    /// Total payload already handed to the publisher.
    published_bytes: usize,
    /// Sealed immutable report retained across bounded writer backpressure.
    sealed: Option<ToolOutputReport>,
    /// VCR-only retained batches and their recording origin.
    recording: Option<(Instant, OutputRecording)>,
    /// Pre-effect bounded publisher registration, released before final flush.
    capacity: Option<tau_client::DetachedCapacityToken>,
    /// Coalesced retry readiness; pipe reads alone never retry a sealed frame.
    capacity_wake: Arc<CapacityWake>,
    /// Pre-effect capacity wake pipe, also retained by its nonblocking
    /// callback.
    #[cfg(unix)]
    capacity_pipe: Arc<CapacityPipe>,
}

impl OutputStreamCapture {
    /// Start a silent stream; no deadline exists until the first byte arrives.
    pub(crate) fn new(
        output: Output,
        call_id: tau_proto::ToolCallId,
        tool_name: tau_proto::ToolName,
    ) -> tau_client::ClientResult<Self> {
        let capacity = output.reserve_output_capacity()?;
        let capacity_wake = Arc::new(CapacityWake {
            ready: AtomicBool::new(false),
            notify: Mutex::new(None),
        });
        #[cfg(unix)]
        let capacity_pipe = {
            use std::os::fd::AsRawFd;
            let pipe = super::ShellWaitWakePipe::open().ok_or_else(|| {
                tau_client::ClientError::handler("cannot allocate output capacity wake pipe")
            })?;
            // Both endpoints must be nonblocking before the callback can run.
            for fd in [&pipe.read, &pipe.write] {
                #[allow(unsafe_code)]
                let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
                #[allow(unsafe_code)]
                let result =
                    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
                if flags < 0 || result < 0 {
                    return Err(tau_client::ClientError::handler(
                        "cannot make output capacity wake pipe nonblocking",
                    ));
                }
            }
            let pipe = Arc::new(CapacityPipe { pipe });
            *capacity_wake.notify.lock().expect("capacity notifier") =
                Some(Waker::from(Arc::clone(&pipe)));
            pipe
        };
        if let Some(token) = &capacity {
            token.set_waker(Waker::from(Arc::clone(&capacity_wake)));
        }
        Ok(Self {
            output,
            call_id,
            tool_name,
            sequence: 0,
            pending: PendingOutput::default(),
            first: None,
            last: None,
            stdout_carry: Vec::new(),
            stderr_carry: Vec::new(),
            published_bytes: 0,
            sealed: None,
            recording: None,
            capacity,
            capacity_wake,
            #[cfg(unix)]
            capacity_pipe,
        })
    }

    /// Connect capacity availability to the non-Unix command coordinator.
    #[cfg(not(unix))]
    pub(super) fn set_capacity_waker(&self, waker: Waker) {
        *self.capacity_wake.notify.lock().expect("capacity notifier") = Some(waker);
    }

    /// Return the preallocated wake endpoint polled beside stdout and stderr.
    #[cfg(unix)]
    pub(super) fn capacity_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        self.capacity_pipe.pipe.read.as_raw_fd()
    }

    /// Reuse the pre-effect endpoint for child exit and cancellation, so a
    /// separate lifecycle-pipe allocation cannot strand the command poll.
    #[cfg(unix)]
    pub(super) fn lifecycle_waker(&self) -> Waker {
        Waker::from(Arc::clone(&self.capacity_pipe))
    }

    /// Clear the shared readiness edge before checking lifecycle queues. Never
    /// drain it between the last lifecycle check and the blocking poll.
    #[cfg(unix)]
    pub(super) fn drain_wake(&self) {
        super::shell_wait_drain_wake_fd(&self.capacity_pipe.pipe.read);
    }

    /// Record only successfully admitted batches with their live relative
    /// times.
    pub(super) fn record_to(&mut self, started: Instant, recording: OutputRecording) {
        self.recording = Some((started, recording));
    }

    fn record_sent(&self, report: &ToolOutputReport, now: Instant) {
        if let Some((started, recording)) = &self.recording {
            recording
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(RecordedShellUpdate {
                    text: report.text.clone(),
                    omitted_bytes: report.omitted_bytes,
                    elapsed_ms: super::elapsed_millis(now.saturating_duration_since(*started)),
                });
        }
    }

    /// Replay a previously accepted bounded report without recapturing it or
    /// starting a new cadence. The caller validates the recorded batch series.
    pub(super) fn replay(&mut self, update: RecordedShellUpdate) {
        let report = ToolOutputReport {
            call_id: self.call_id.clone(),
            tool_name: self.tool_name.clone(),
            sequence: self.sequence,
            text: update.text,
            omitted_bytes: update.omitted_bytes,
        };
        let _ = self
            .output
            .send_checked(HarnessInputMessage::emit_with_persist(
                Event::ToolOutputReported(report),
                false,
            ));
        self.sequence += 1;
    }

    /// Observe bytes even after the text budget fills; activity never emits
    /// early.
    pub(super) fn push(&mut self, stream: OutputStream, bytes: &[u8], now: Instant) {
        if bytes.is_empty() {
            return;
        }
        self.first.get_or_insert(now);
        self.last = Some(now);
        let stderr = matches!(stream, OutputStream::Stderr);
        let carry = if stderr {
            &mut self.stderr_carry
        } else {
            &mut self.stdout_carry
        };
        let mut input = std::mem::take(carry);
        input.extend_from_slice(bytes);
        let mut remainder = input.as_slice();
        let mut decoded = String::new();
        loop {
            match std::str::from_utf8(remainder) {
                Ok(text) => {
                    decoded.push_str(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    decoded.push_str(
                        std::str::from_utf8(&remainder[..valid]).expect("valid UTF-8 prefix"),
                    );
                    match error.error_len() {
                        Some(invalid) => {
                            decoded.push('\u{fffd}');
                            remainder = &remainder[valid + invalid..];
                        }
                        None => {
                            carry.extend_from_slice(&remainder[valid..]);
                            break;
                        }
                    }
                }
            }
        }
        self.pending.push(stderr, &decoded);
    }

    /// The first of idle and absolute deadlines for the pending batch.
    pub(super) fn deadline(&self) -> Option<Instant> {
        if self.sealed.is_some() {
            return None;
        }
        if self.published_bytes >= tau_proto::MAX_TOOL_OUTPUT_LIVE_BYTES
            || self.sequence >= tau_proto::MAX_TOOL_OUTPUT_UPDATES
        {
            return None;
        }
        Some((self.first? + Duration::from_secs(30)).min(self.last? + Duration::from_secs(15)))
    }

    /// Offer due output without blocking pipe draining on protocol I/O.
    pub(super) fn poll(&mut self, now: Instant) {
        if let Some(report) = self.sealed.as_ref() {
            if !self.capacity_wake.ready.swap(false, Ordering::AcqRel) {
                return;
            }
            let sent = self.output.send_or_arm(
                HarnessInputMessage::emit_with_persist(
                    Event::ToolOutputReported(report.clone()),
                    false,
                ),
                self.capacity.as_ref(),
            );
            match sent {
                Ok(()) => {
                    self.record_sent(report, now);
                    self.published_bytes += report.text.len();
                    self.sequence += 1;
                    self.sealed = None;
                }
                Err(tau_client::ClientError::Overloaded) => return,
                Err(error) => {
                    let _ = self.output.retain_mandatory_failure(Err(error));
                    return;
                }
            }
        }
        if !self.deadline().is_some_and(|deadline| deadline <= now) {
            return;
        }
        let available = tau_proto::MAX_TOOL_OUTPUT_LIVE_BYTES.saturating_sub(self.published_bytes);
        let (text, omitted_bytes) = self.pending.take(available);
        if text.is_empty() && omitted_bytes == 0 {
            // An incomplete codepoint is not an empty output notification.
            self.first = None;
            self.last = None;
            return;
        }
        self.sealed = Some(ToolOutputReport {
            call_id: self.call_id.clone(),
            tool_name: self.tool_name.clone(),
            sequence: self.sequence,
            text,
            omitted_bytes,
        });
        self.first = None;
        self.last = None;
        self.capacity_wake.ready.store(true, Ordering::Release);
        self.poll(now);
    }

    /// Flush previously sealed output first, then return only the not-yet-sent
    /// tail. Waiting for writer flush is safe here: the process drain has
    /// already ended.
    pub(super) fn finish(&mut self) -> String {
        self.capacity = None;
        if let Some(report) = self.sealed.take() {
            if self
                .output
                .send_checked(HarnessInputMessage::emit_with_persist(
                    Event::ToolOutputReported(report.clone()),
                    false,
                ))
                .is_ok()
            {
                self.record_sent(&report, Instant::now());
            }
            self.sequence += 1;
        }
        for stderr in [false, true] {
            let carry = if stderr {
                &mut self.stderr_carry
            } else {
                &mut self.stdout_carry
            };
            if !carry.is_empty() {
                carry.clear();
                self.pending.push(stderr, "\u{fffd}");
            }
        }
        let (mut text, omitted) = self.pending.take(tau_proto::MAX_TOOL_OUTPUT_CHUNK_BYTES);
        if omitted != 0 {
            text.insert_str(
                0,
                &format!("[{omitted} rendered bytes omitted before retained terminal tail]\n"),
            );
        }
        text
    }

    /// Whether the ordinary terminal must avoid repeating earlier batches.
    pub(super) fn emitted(&self) -> bool {
        self.sequence != 0 || self.sealed.is_some()
    }
}

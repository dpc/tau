//! Internal harness event type and the per-connection reader/writer threads
//! that funnel decoded protocol events into the central event loop.

use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tau_client::ProtocolIoMeter;
use tau_core::{ConnectionSendError, ConnectionSink};
use tau_proto::{
    Disconnect, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
};

use crate::extension::ExtensionConnectCommand;

/// Grace period before a blocked supervised writer is forcefully unblocked.
pub(crate) const SUPERVISED_CLEANUP_GRACE: Duration = Duration::from_secs(2);

/// Commands that mutate harness-owned state from inside the central loop.
pub(crate) enum HarnessCommand {
    /// Install a spawned extension connection and release its reader.
    ConnectExtension(Box<ExtensionConnectCommand>),
    /// Complete an asynchronous external message tool delivery attempt.
    ExternalMessageToolCompleted(Box<ExternalMessageToolCompletedCommand>),
    /// Complete receiver-side authentication for an inbound external message.
    ExternalMessageAuthCompleted(Box<ExternalMessageAuthCompletedCommand>),
    /// Complete one bounded peer-session discovery tool call.
    SessionDiscoveryCompleted(Box<SessionDiscoveryCompletedCommand>),
}

/// Completion payload for asynchronous peer-session discovery.
pub(crate) struct SessionDiscoveryCompletedCommand {
    /// Conversation that owns the tool call.
    pub(crate) conversation_id: tau_proto::AgentId,
    /// Session generation active when discovery began.
    pub(crate) session_generation: u64,
    /// Tool call id being completed.
    pub(crate) call_id: tau_proto::ToolCallId,
    /// Visible tool name.
    pub(crate) tool_name: tau_proto::ToolName,
    /// Provider-declared tool type.
    pub(crate) tool_type: tau_proto::ToolType,
    /// Structured redacted discovery result.
    pub(crate) result: tau_proto::CborValue,
}

/// Completion payload for asynchronous external message tool delivery.
pub(crate) struct ExternalMessageToolCompletedCommand {
    /// Global outbound-I/O admission retained until event-loop consumption.
    pub(crate) _permit: Option<crate::harness::PeerIoPermit>,
    /// Conversation that owns the tool call.
    pub(crate) conversation_id: tau_proto::AgentId,
    /// Session generation that was active when the external message tool call
    /// started.
    pub(crate) session_generation: u64,
    /// Tool call id being completed.
    pub(crate) call_id: tau_proto::ToolCallId,
    /// Visible tool name for terminal result/error events.
    pub(crate) tool_name: tau_proto::ToolName,
    /// Tool type declared by the provider.
    pub(crate) tool_type: tau_proto::ToolType,
    /// Resolved recipient and started flag on delivery success.
    pub(crate) result: Result<(tau_proto::AgentId, bool), String>,
    /// Original call arguments for error details.
    pub(crate) details: tau_proto::CborValue,
    /// Pending sender-authentication entry to remove when the async attempt
    /// ends.
    pub(crate) auth_message_id: tau_proto::AgentMessageId,
    /// Whether an ordinary message needs a sender-side durable projection.
    pub(crate) publish_sent: bool,
    /// Harness-authored sender id for the eventual projection.
    pub(crate) sender_id: tau_proto::AgentId,
    /// Target session for the eventual resolved projection.
    pub(crate) recipient_session_id: tau_proto::SessionId,
    /// Delivery kind.
    pub(crate) kind: tau_proto::AgentMessageKind,
    /// Message body.
    pub(crate) message: String,
}

/// Completion payload for receiver-side external-message authentication.
pub(crate) struct ExternalMessageAuthCompletedCommand {
    /// Global inbound-I/O admission retained until event-loop consumption.
    pub(crate) _permit: Option<crate::harness::PeerIoPermit>,
    /// Socket client that sent the external message RPC.
    pub(crate) client_id: tau_proto::ConnectionId,
    /// Session generation in which callback authentication began.
    pub(crate) session_generation: u64,
    /// Request to publish after successful sender authentication.
    pub(crate) request: tau_proto::ExternalAgentMessageRequest,
    /// Authentication result from the helper thread.
    pub(crate) result: Result<(), String>,
}

/// Internal event type — all reader threads feed this into one channel.
pub(crate) enum HarnessEvent {
    /// Decoded harness input message from any connection (extension or client).
    FromConnection {
        connection_id: tau_proto::ConnectionId,
        message: Box<HarnessInputMessage>,
        /// Encoded bytes consumed by the real protocol decode.
        frame_bytes: tau_proto::ProtocolMessageBytes,
    },
    /// A connection's reader hit clean EOF.
    Disconnected {
        connection_id: tau_proto::ConnectionId,
    },
    /// A connection's reader rejected a malformed or oversized protocol frame.
    ReadFailed {
        connection_id: tau_proto::ConnectionId,
        error: String,
    },
    /// Socket listener accepted a new client.
    NewClient(UnixStream),
    /// A supervised writer finished and reaped its owned direct child.
    SupervisedWriterCleanupComplete {
        connection_id: tau_proto::ConnectionId,
    },
    /// Internal state transition requested by harness helpers.
    Command(HarnessCommand),
}

impl HarnessEvent {
    /// Build a synthetic connection event while accounting for its actual
    /// encoded fixture size.
    #[cfg(test)]
    pub(crate) fn from_connection_for_test(
        connection_id: tau_proto::ConnectionId,
        message: HarnessInputMessage,
    ) -> Self {
        let frame_bytes = tau_proto::encode_message_to_vec(&message)
            .ok()
            .and_then(|encoded| tau_proto::ProtocolMessageBytes::new(encoded.len() as u64))
            .expect("a synthetic harness input message encodes to a nonempty frame");
        Self::FromConnection {
            connection_id,
            message: Box::new(message),
            frame_bytes,
        }
    }
}

/// Commands accepted by per-connection writer threads.
pub(crate) enum WriterCommand {
    /// Write one protocol frame to the connection.
    Message(HarnessOutputMessage),
    /// Flush all previously queued frames, then acknowledge completion.
    Flush(Sender<()>),
}

/// Connection sink — sends to the per-connection writer channel.
pub(crate) struct ChannelSink {
    pub(crate) tx: Sender<WriterCommand>,
}

impl ConnectionSink for ChannelSink {
    fn send(&mut self, routed: tau_core::RoutedFrame) -> Result<(), ConnectionSendError> {
        self.tx
            .send(WriterCommand::Message(routed.frame))
            .map_err(|_| ConnectionSendError::new("writer closed"))
    }
}

/// Reader thread — one per connection, sends to the shared harness channel.
pub(crate) fn spawn_reader_thread(
    connection_id: tau_proto::ConnectionId,
    stream: impl io::Read + Send + 'static,
    tx: Sender<HarnessEvent>,
) {
    spawn_reader_thread_inner(connection_id, stream, tx, None);
}

/// Reader thread for extensions whose messages must not enter the harness loop
/// until the harness has created all matching connection and lifecycle state.
pub(crate) fn spawn_reader_thread_after_initialized(
    connection_id: tau_proto::ConnectionId,
    stream: impl io::Read + Send + 'static,
    tx: Sender<HarnessEvent>,
    initialized_rx: Receiver<()>,
) {
    spawn_reader_thread_inner(connection_id, stream, tx, Some(initialized_rx));
}

fn spawn_reader_thread_inner(
    connection_id: tau_proto::ConnectionId,
    stream: impl io::Read + Send + 'static,
    tx: Sender<HarnessEvent>,
    initialized_rx: Option<Receiver<()>>,
) {
    thread::spawn(move || {
        if let Some(initialized_rx) = initialized_rx
            && initialized_rx.recv().is_err()
        {
            return;
        }

        let mut reader = HarnessInputReader::new(BufReader::new(stream));
        loop {
            match reader.read_message_with_size() {
                Ok(Some(decoded)) => {
                    if tx
                        .send(HarnessEvent::FromConnection {
                            connection_id: connection_id.clone(),
                            message: Box::new(decoded.message),
                            frame_bytes: decoded.encoded_bytes,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(HarnessEvent::Disconnected {
                        connection_id: connection_id.clone(),
                    });
                    return;
                }
                Err(error) => {
                    let _ = tx.send(HarnessEvent::ReadFailed {
                        connection_id: connection_id.clone(),
                        error: error.to_string(),
                    });
                    return;
                }
            }
        }
    });
}

/// Cleanup ownership selected when a writer thread is spawned.
enum WriterShutdown {
    /// Close only the transport stream.
    CloseStream,
    /// Close child stdin, reap the child, and report completion.
    Supervised {
        /// Direct child owned exclusively by the writer until it is reaped.
        child: Child,
        /// Harness notification and outer blocked-write watchdog state.
        completion: SupervisedWriterCompletion,
    },
}

/// Join and watchdog state retained by the harness for a supervised writer.
pub(crate) struct SupervisedWriterHandle {
    /// Recorded direct-child PID, valid until the writer-owned child is reaped.
    child_pid: u32,
    /// Writer thread that owns the child and its final wait/reap path.
    writer_thread: Option<JoinHandle<()>>,
    /// Shared two-phase watchdog transition synchronized with child reaping.
    watchdog: Arc<WriterWatchdog>,
}

impl SupervisedWriterHandle {
    /// Signal the recorded direct child if the writer has not reached its
    /// ordinary child-reaping path.
    pub(crate) fn fire_watchdog(&self) {
        self.watchdog.fire(self.child_pid);
    }

    /// Record the absolute disconnect-to-kill deadline used by both cleanup
    /// phases.
    pub(crate) fn arm_cleanup_deadline(&self, deadline: Instant) {
        self.watchdog.arm_deadline(deadline);
    }

    /// Start a deadline watcher used while the central event loop is no longer
    /// running during harness shutdown.
    pub(crate) fn start_shutdown_watchdog(&self) -> JoinHandle<()> {
        let watchdog = Arc::clone(&self.watchdog);
        let child_pid = self.child_pid;
        let deadline = watchdog.arm_deadline(Instant::now() + SUPERVISED_CLEANUP_GRACE);
        thread::spawn(move || {
            watchdog.wait_and_fire(child_pid, deadline);
        })
    }

    /// Join the writer after cleanup completion or during coordinated shutdown.
    pub(crate) fn join(&mut self) -> thread::Result<()> {
        self.writer_thread.take().map_or(Ok(()), JoinHandle::join)
    }

    #[cfg(test)]
    /// Whether the outer blocked-write watchdog signaled the child.
    pub(crate) fn watchdog_fired(&self) -> bool {
        self.watchdog
            .state
            .lock()
            .expect("writer watchdog state")
            .phase
            == WriterWatchdogPhase::Fired
    }
}

impl Drop for SupervisedWriterHandle {
    fn drop(&mut self) {
        if self.writer_thread.is_none() {
            return;
        }
        let watchdog = self.start_shutdown_watchdog();
        let _ = self.join();
        let _ = watchdog.join();
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
/// Exclusive relationship between the outer watchdog and writer reap path.
enum WriterWatchdogPhase {
    /// The writer may still be blocked before its child wait/reap path.
    Pending,
    /// The writer reached its child wait/reap path before the outer deadline.
    Canceled,
    /// The outer deadline signaled the still-unreaped direct child.
    Fired,
}

/// State protected by the watchdog transition mutex.
struct WriterWatchdogState {
    /// Exclusive signaling/cancellation phase.
    phase: WriterWatchdogPhase,
    /// Absolute disconnect-to-kill deadline, once the harness arms cleanup.
    deadline: Option<Instant>,
}

/// Synchronizes the outer blocked-write watchdog with writer-owned child reap.
struct WriterWatchdog {
    /// Current exclusive transition state.
    state: Mutex<WriterWatchdogState>,
    /// Wakes shutdown watchdog threads when normal cleanup wins.
    changed: Condvar,
}

impl WriterWatchdog {
    fn new() -> Self {
        Self {
            state: Mutex::new(WriterWatchdogState {
                phase: WriterWatchdogPhase::Pending,
                deadline: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn arm_deadline(&self, deadline: Instant) -> Instant {
        let mut state = self.state.lock().expect("writer watchdog state");
        if state.phase == WriterWatchdogPhase::Pending {
            state.deadline.get_or_insert(deadline);
        }
        state.deadline.unwrap_or(deadline)
    }

    fn cancel_and_deadline(&self, fallback: Instant) -> Instant {
        let mut state = self.state.lock().expect("writer watchdog state");
        if state.phase == WriterWatchdogPhase::Pending {
            state.phase = WriterWatchdogPhase::Canceled;
            self.changed.notify_all();
        }
        state.deadline.unwrap_or(fallback)
    }

    fn fire(&self, child_pid: u32) {
        let mut state = self.state.lock().expect("writer watchdog state");
        if state.phase != WriterWatchdogPhase::Pending {
            return;
        }
        // SAFETY: the writer owns the unreaped direct Child until it cancels
        // this watchdog under the same mutex immediately before entering its
        // wait/reap path. Winning this mutex therefore proves this PID cannot
        // have been reaped and reused.
        #[allow(unsafe_code)]
        unsafe {
            libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
        }
        state.phase = WriterWatchdogPhase::Fired;
        self.changed.notify_all();
    }

    fn wait_and_fire(&self, child_pid: u32, deadline: Instant) {
        let state = self.state.lock().expect("writer watchdog state");
        let (state, _) = self
            .changed
            .wait_timeout_while(
                state,
                deadline.saturating_duration_since(Instant::now()),
                |state| state.phase == WriterWatchdogPhase::Pending,
            )
            .expect("writer watchdog wait");
        if state.phase == WriterWatchdogPhase::Pending {
            drop(state);
            self.fire(child_pid);
        }
    }
}

/// Completion data moved into exactly one supervised writer thread.
struct SupervisedWriterCompletion {
    /// Connection whose cleanup ownership is completing.
    connection_id: tau_proto::ConnectionId,
    /// Central harness event sender used after the child has been reaped.
    harness_tx: Sender<HarnessEvent>,
    /// Outer watchdog canceled before the writer enters child wait/reap.
    watchdog: Arc<WriterWatchdog>,
}

/// Writer thread — one per connection, drains channel and writes to stream.
pub(crate) fn spawn_writer_thread(
    writer: impl Write + Send + 'static,
    protocol_io: Option<ProtocolIoMeter>,
) -> Sender<WriterCommand> {
    let (tx, _) = spawn_writer_thread_inner(writer, WriterShutdown::CloseStream, protocol_io);
    tx
}

/// Spawn a supervised writer whose join handle remains owned by the harness.
pub(crate) fn spawn_supervised_writer_thread(
    connection_id: tau_proto::ConnectionId,
    writer: impl Write + Send + 'static,
    child: Child,
    protocol_io: Option<ProtocolIoMeter>,
    harness_tx: Sender<HarnessEvent>,
) -> (Sender<WriterCommand>, SupervisedWriterHandle) {
    let child_pid = child.id();
    let watchdog = Arc::new(WriterWatchdog::new());
    let completion = SupervisedWriterCompletion {
        connection_id,
        harness_tx,
        watchdog: Arc::clone(&watchdog),
    };
    let (tx, writer_thread) = spawn_writer_thread_inner(
        writer,
        WriterShutdown::Supervised { child, completion },
        protocol_io,
    );
    (
        tx,
        SupervisedWriterHandle {
            child_pid,
            writer_thread: Some(writer_thread),
            watchdog,
        },
    )
}

fn spawn_writer_thread_inner(
    writer: impl Write + Send + 'static,
    shutdown: WriterShutdown,
    protocol_io: Option<ProtocolIoMeter>,
) -> (Sender<WriterCommand>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<WriterCommand>();
    let writer_thread = thread::spawn(move || {
        let mut w = HarnessOutputWriter::new(BufWriter::new(writer));
        // Drain output messages until the channel closes. Write failures still
        // fall through to the shutdown sequence so supervised children are
        // reaped instead of being abandoned after stdin breaks.
        let mut can_write_disconnect = true;
        while let Ok(command) = rx.recv() {
            match command {
                WriterCommand::Message(message) => {
                    let Ok(frame_bytes) = w.write_message_with_size(&message) else {
                        can_write_disconnect = false;
                        break;
                    };
                    if w.flush().is_err() {
                        can_write_disconnect = false;
                        break;
                    }
                    if let Some(protocol_io) = &protocol_io {
                        protocol_io.record_downlink_frame_bytes(&message, frame_bytes);
                    }
                }
                WriterCommand::Flush(ack) => {
                    let _ = w.flush();
                    let _ = ack.send(());
                }
            }
        }

        // Channel closed or writer failed — run shutdown sequence.
        match shutdown {
            WriterShutdown::CloseStream => {
                // Drop the writer → closes the stream.
            }
            WriterShutdown::Supervised { child, completion } => {
                if can_write_disconnect {
                    // Best-effort disconnect message.
                    let disconnect = HarnessOutputMessage::Disconnect(Disconnect {
                        reason: Some("shutdown".to_owned()),
                    });
                    if let Ok(frame_bytes) = w.write_message_with_size(&disconnect)
                        && w.flush().is_ok()
                        && let Some(protocol_io) = &protocol_io
                    {
                        protocol_io.record_downlink_frame_bytes(&disconnect, frame_bytes);
                    }
                }
                // Drop the writer → closes stdin → extension sees EOF.
                drop(w);
                // The outer watchdog exists only to unblock a writer stuck
                // before this point. Cancel it under the same mutex that guards
                // signaling; the writer now exclusively owns the bounded child
                // wait/reap phase below.
                let deadline = completion
                    .watchdog
                    .cancel_and_deadline(Instant::now() + SUPERVISED_CLEANUP_GRACE);
                wait_until_deadline(child, deadline);
                let _ = completion
                    .harness_tx
                    .send(HarnessEvent::SupervisedWriterCleanupComplete {
                        connection_id: completion.connection_id,
                    });
            }
        }
    });
    (tx, writer_thread)
}

/// Block until `child` exits, or escalate to `SIGKILL` at `deadline`.
///
/// A helper observes direct-child exit with `waitid(WNOWAIT)` so it never
/// reaps. The writer retains the [`Child`] and performs both forced termination
/// and the only reap, preventing PID reuse between the deadline and signal.
fn wait_until_deadline(mut child: Child, deadline: Instant) {
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel::<io::Result<()>>();
    let observer = thread::spawn(move || {
        let observation = loop {
            let mut status = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            // SAFETY: `status` points to writable siginfo storage. `WNOWAIT`
            // observes this direct child's exit without reaping it, so the
            // writer's owned `Child` remains authoritative until
            // `child.wait()` below.
            #[allow(unsafe_code)]
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    status.as_mut_ptr(),
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if result == 0 {
                break Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                break Err(error);
            }
        };
        let _ = done_tx.send(observation);
    });
    if !matches!(
        done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())),
        Ok(Ok(()))
    ) {
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = observer.join();
}

#[cfg(test)]
mod tests;

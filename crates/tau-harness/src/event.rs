//! Internal harness event type and the per-connection reader/writer threads
//! that funnel decoded protocol events into the central event loop.

use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use tau_client::ProtocolIoMeter;
use tau_core::{ConnectionSendError, ConnectionSink};
use tau_proto::{
    Disconnect, HarnessInputMessage, HarnessInputReader, HarnessOutputMessage, HarnessOutputWriter,
};

use crate::extension::ExtensionConnectCommand;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

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
    /// Internal state transition requested by harness helpers.
    Command(HarnessCommand),
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
            match reader.read_message() {
                Ok(Some(message)) => {
                    if tx
                        .send(HarnessEvent::FromConnection {
                            connection_id: connection_id.clone(),
                            message: Box::new(message),
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

/// What the writer thread should do when its channel closes.
pub(crate) enum WriterShutdown {
    /// Just close the stream (socket clients, in-process peers).
    CloseStream,
    /// Supervised child: send disconnect, close stdin, wait/signal.
    KillChild(Child),
}

/// Writer thread — one per connection, drains channel and writes to stream.
pub(crate) fn spawn_writer_thread(
    writer: impl Write + Send + 'static,
    shutdown: WriterShutdown,
    protocol_io: Option<ProtocolIoMeter>,
) -> Sender<WriterCommand> {
    let (tx, rx) = mpsc::channel::<WriterCommand>();
    thread::spawn(move || {
        let mut w = HarnessOutputWriter::new(BufWriter::new(writer));
        // Drain output messages until the channel closes. Write failures still
        // fall through to the shutdown sequence so supervised children are
        // reaped instead of being abandoned after stdin breaks.
        let mut can_write_disconnect = true;
        while let Ok(command) = rx.recv() {
            match command {
                WriterCommand::Message(message) => {
                    if w.write_message(&message).is_err() || w.flush().is_err() {
                        can_write_disconnect = false;
                        break;
                    }
                    if let Some(protocol_io) = &protocol_io {
                        protocol_io.record_downlink_frame(&message);
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
            WriterShutdown::KillChild(child) => {
                if can_write_disconnect {
                    // Best-effort disconnect message.
                    let disconnect = HarnessOutputMessage::Disconnect(Disconnect {
                        reason: Some("shutdown".to_owned()),
                    });
                    if w.write_message(&disconnect).is_ok()
                        && w.flush().is_ok()
                        && let Some(protocol_io) = &protocol_io
                    {
                        protocol_io.record_downlink_frame(&disconnect);
                    }
                }
                // Drop the writer → closes stdin → extension sees EOF.
                drop(w);

                wait_with_grace(child, SHUTDOWN_GRACE);
            }
        }
    });
    tx
}

/// Block until `child` exits, or escalate to `SIGKILL` after `grace`.
///
/// The wait happens on a helper thread so the caller can time it out via a
/// channel rather than polling `try_wait`. On timeout we signal the child
/// by PID; the helper thread's `wait()` then reaps it.
fn wait_with_grace(mut child: Child, grace: Duration) {
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let waiter = thread::spawn(move || {
        let _ = child.wait();
        let _ = done_tx.send(());
    });
    if done_rx.recv_timeout(grace).is_err() {
        // SAFETY: signaling a process by PID. The PID cannot be recycled until
        // the helper thread's `wait()` reaps the child, which has not happened
        // yet (we just timed out waiting for it).
        #[allow(unsafe_code)]
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
        let _ = done_rx.recv();
    }
    let _ = waiter.join();
}

#[cfg(test)]
mod tests;

//! Internal harness event type and the per-connection reader/writer threads
//! that funnel decoded protocol events into the central event loop.

#[cfg(test)]
use std::io::BufReader;
use std::io::{self, BufWriter, Write};
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
    pub(crate) result: Result<(tau_proto::AgentId, bool), ExternalMessageDeliveryError>,
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

/// Sender-side outcome of one cross-harness delivery attempt.
///
/// Local failures retain their existing diagnostic, while target failures carry
/// only a protocol-defined classification and are rendered as fixed local text.
pub(crate) enum ExternalMessageDeliveryError {
    /// A local lookup, connection, or deadline failure.
    Local(String),
    /// A sanitized rejection classification returned by the target harness.
    Target(tau_proto::ExternalAgentMessageFailure),
}

impl ExternalMessageDeliveryError {
    /// Return the fixed caller-visible message for this delivery failure.
    pub(crate) fn tool_message(self) -> String {
        match self {
            Self::Local(message) => message,
            Self::Target(tau_proto::ExternalAgentMessageFailure::TargetSessionChanged) => {
                "target session changed before message delivery; retry".to_owned()
            }
            Self::Target(tau_proto::ExternalAgentMessageFailure::NoInterSessionReceiver) => {
                "target live; no receiver; set `inter_session_receiver`".to_owned()
            }
            Self::Target(tau_proto::ExternalAgentMessageFailure::RecipientStopped) => {
                "target recipient is stopped; start a replacement and retry".to_owned()
            }
            Self::Target(tau_proto::ExternalAgentMessageFailure::RecipientRestoredUnavailable) => {
                "target recipient cannot resume its pre-restart delegation; \
                 start a replacement and retry"
                    .to_owned()
            }
            Self::Target(tau_proto::ExternalAgentMessageFailure::RecipientUnknown) => {
                "target recipient is unknown; choose a live recipient and retry".to_owned()
            }
            Self::Target(tau_proto::ExternalAgentMessageFailure::Rejected) => {
                "target session rejected the inter-session message".to_owned()
            }
        }
    }
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
    /// Non-payload control wake indicating bounded component ingress is ready.
    ComponentIngressReady,
}

impl HarnessEvent {
    /// Returns the fixed local prompt-traffic class, if this is a prompt frame.
    pub(crate) fn prompt_traffic_class(&self) -> Option<&'static str> {
        let Self::FromConnection { message, .. } = self else {
            return None;
        };
        prompt_traffic_class_for_message(message)
    }
}

/// Classifies only canonical UI prompt-bearing input without cloning payloads.
fn prompt_traffic_class_for_message(message: &HarnessInputMessage) -> Option<&'static str> {
    let HarnessInputMessage::Emit(emit) = message else {
        return None;
    };
    match emit.event.as_ref() {
        tau_proto::Event::UiPromptSubmitted(_) => Some("ui_prompt_submitted"),
        tau_proto::Event::UiCreateAgent(request) if request.initial_prompt.is_some() => {
            Some("ui_create_agent")
        }
        _ => None,
    }
}

/// Shared state for the trivially small component-ingress rendezvous lane.
struct ComponentIngressState {
    /// At most one component payload awaiting harness consumption.
    slot: Option<PendingIngress>,
    /// Whether the harness still accepts component ingress.
    receiver_alive: bool,
    /// Producers currently waiting behind an occupied one-slot lane.
    blocked_senders: usize,
    /// Next sender-specific acknowledgement ticket.
    next_ticket: IngressTicket,
    /// Greatest ticket whose exact payload the harness consumed.
    consumed_through: Option<IngressTicket>,
}

/// Sender-specific identity for one component-ingress payload.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct IngressTicket(
    /// Monotonic owner-local ticket value.
    u64,
);

impl IngressTicket {
    /// Returns the following owner-local ticket.
    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// One payload occupying the shared component-ingress slot.
struct PendingIngress {
    /// Sender-specific acknowledgement identity.
    ticket: IngressTicket,
    /// Component payload awaiting harness consumption.
    event: HarnessEvent,
    /// Local monotonic instant when this payload occupied the slot.
    occupied_at: Instant,
    /// Fixed local prompt class, or none for unrelated ingress.
    prompt_traffic_class: Option<&'static str>,
}

/// Captured prompt timing emitted only after the ingress lock is released.
pub(crate) struct IngressTakeDiagnostic {
    /// Fixed content-free prompt traffic class.
    traffic_class: &'static str,
    /// Slot occupation through payload take, in microseconds.
    wake_to_take_ready_us: u128,
    /// Exact blocked-sender count observed while taking the payload.
    blocked_reader_count: usize,
}

/// One taken payload plus optional prompt-only timing metadata.
pub(crate) struct TakenIngress {
    /// Component payload released from the one-slot lane.
    pub(crate) event: HarnessEvent,
    /// Captured prompt timing, absent for unrelated traffic.
    pub(crate) diagnostic: Option<IngressTakeDiagnostic>,
}

/// Harness-owned receiver for bounded or rendezvous component ingress.
pub(crate) struct ComponentIngress {
    /// Shared slot and lifecycle state.
    state: Arc<(Mutex<ComponentIngressState>, Condvar)>,
}

/// Cloneable producer for bounded or rendezvous component ingress.
#[derive(Clone)]
pub(crate) struct ComponentIngressSender {
    /// Shared slot and lifecycle state.
    state: Arc<(Mutex<ComponentIngressState>, Condvar)>,
    /// Control-lane wake sender; it never carries the component payload.
    wake_tx: Sender<HarnessEvent>,
    /// Selected rendezvous or one-slot backpressure behavior.
    capacity: ComponentIngressCapacity,
}

/// Supported component-ingress backpressure modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComponentIngressCapacity {
    /// Producer completes only after harness consumption.
    Rendezvous,
    /// One payload may wait for harness consumption.
    One,
}

impl ComponentIngress {
    /// Creates a component ingress lane correct at capacities zero and one.
    pub(crate) fn new(
        wake_tx: Sender<HarnessEvent>,
        capacity: ComponentIngressCapacity,
    ) -> (Self, ComponentIngressSender) {
        let state = Arc::new((
            Mutex::new(ComponentIngressState {
                slot: None,
                receiver_alive: true,
                blocked_senders: 0,
                next_ticket: IngressTicket(1),
                consumed_through: None,
            }),
            Condvar::new(),
        ));
        (
            Self {
                state: Arc::clone(&state),
            },
            ComponentIngressSender {
                state,
                wake_tx,
                capacity,
            },
        )
    }

    /// Takes the payload associated with one control-lane wake.
    #[cfg(test)]
    pub(crate) fn take_ready(&self) -> Option<HarnessEvent> {
        self.take_ready_with_diagnostic().map(|taken| {
            Self::trace_take_diagnostic(taken.diagnostic);
            taken.event
        })
    }

    /// Takes one payload while deferring diagnostic emission to the caller.
    pub(crate) fn take_ready_with_diagnostic(&self) -> Option<TakenIngress> {
        let (state, changed) = &*self.state;
        let mut state = state.lock().expect("component ingress mutex poisoned");
        let taken = state.slot.take().map(|pending| {
            let diagnostic =
                pending
                    .prompt_traffic_class
                    .map(|traffic_class| IngressTakeDiagnostic {
                        traffic_class,
                        wake_to_take_ready_us: pending.occupied_at.elapsed().as_micros(),
                        blocked_reader_count: state.blocked_senders,
                    });
            state.consumed_through = Some(pending.ticket);
            TakenIngress {
                event: pending.event,
                diagnostic,
            }
        });
        changed.notify_all();
        drop(state);
        taken
    }

    /// Emits already-captured take diagnostics after semantic state is
    /// released.
    pub(crate) fn trace_take_diagnostic(diagnostic: Option<IngressTakeDiagnostic>) {
        if let Some(diagnostic) = diagnostic {
            tracing::trace!(
                target: "tau_harness::prompt_ingress",
                stage = "wake_to_take_ready",
                traffic_class = diagnostic.traffic_class,
                wake_to_take_ready_us = diagnostic.wake_to_take_ready_us,
                blocked_reader_count = diagnostic.blocked_reader_count,
                slot_occupied = true,
                "content-free component ingress stage"
            );
        }
    }

    /// Closes ingress and wakes every producer before component joins begin.
    pub(crate) fn close(&self) {
        let (state, changed) = &*self.state;
        let mut state = state.lock().expect("component ingress mutex poisoned");
        state.receiver_alive = false;
        state.slot = None;
        changed.notify_all();
    }

    /// Waits until one producer demonstrably blocks behind the occupied slot.
    #[cfg(test)]
    fn wait_for_blocked_sender(&self) {
        let (state, changed) = &*self.state;
        let state = state.lock().expect("component ingress mutex poisoned");
        let (state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(1), |state| {
                state.blocked_senders == 0
            })
            .expect("component ingress mutex poisoned while observing saturation");
        assert!(!timeout.timed_out(), "producer did not saturate ingress");
        assert_eq!(state.blocked_senders, 1);
    }
}

impl ComponentIngressSender {
    /// Sends one component frame or lifecycle observation with natural
    /// backpressure from harness consumption.
    fn send(&self, event: HarnessEvent) -> Result<(), ()> {
        let started = Instant::now();
        let (state, changed) = &*self.state;
        let mut state = state.lock().expect("component ingress mutex poisoned");
        let blocked = state.receiver_alive && state.slot.is_some();
        if blocked {
            state.blocked_senders = state.blocked_senders.saturating_add(1);
            changed.notify_all();
        }
        while state.receiver_alive && state.slot.is_some() {
            state = changed
                .wait(state)
                .expect("component ingress mutex poisoned while sending");
        }
        if blocked {
            state.blocked_senders = state.blocked_senders.saturating_sub(1);
        }
        if !state.receiver_alive {
            return Err(());
        }
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.next();
        let prompt_traffic_class = event.prompt_traffic_class();
        state.slot = Some(PendingIngress {
            ticket,
            event,
            occupied_at: Instant::now(),
            prompt_traffic_class,
        });
        let blocked_reader_count = state.blocked_senders;
        drop(state);
        if self
            .wake_tx
            .send(HarnessEvent::ComponentIngressReady)
            .is_err()
        {
            self.close_after_wake_failure();
            return Err(());
        }
        if let Some(traffic_class) = prompt_traffic_class {
            tracing::trace!(
                target: "tau_harness::prompt_ingress",
                stage = "ingress_admission",
                traffic_class,
                ingress_wait_us = started.elapsed().as_micros(),
                blocked_reader_count,
                slot_occupied = true,
                "content-free component ingress stage"
            );
        }
        if self.capacity == ComponentIngressCapacity::Rendezvous {
            let mut state = state_lock(&self.state);
            while state.receiver_alive
                && state
                    .consumed_through
                    .is_none_or(|consumed| consumed < ticket)
            {
                state = changed
                    .wait(state)
                    .expect("component ingress mutex poisoned during rendezvous");
            }
            if state
                .consumed_through
                .is_none_or(|consumed| consumed < ticket)
            {
                return Err(());
            }
        }
        Ok(())
    }

    /// Marks the receiver closed when the control wake lane has disappeared.
    fn close_after_wake_failure(&self) {
        let (state, changed) = &*self.state;
        let mut state = state.lock().expect("component ingress mutex poisoned");
        state.receiver_alive = false;
        state.slot = None;
        changed.notify_all();
    }
}

/// Locks component ingress state without repeating tuple destructuring.
fn state_lock(
    state: &Arc<(Mutex<ComponentIngressState>, Condvar)>,
) -> std::sync::MutexGuard<'_, ComponentIngressState> {
    state.0.lock().expect("component ingress mutex poisoned")
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
    #[cfg(test)]
    Message(HarnessOutputMessage),
    /// Flush all previously queued frames, then acknowledge completion.
    #[cfg(test)]
    Flush(Sender<()>),
    /// Switches this writer permanently to cursor-followed shared delivery.
    Follow {
        /// Shared logical live stream.
        log: Arc<crate::event_log::EventLog>,
        /// Runtime-only consumer generation.
        consumer: tau_core::SharedConsumerId,
        /// Lifecycle/control sender used for explicit writer-failure
        /// retirement.
        failure_tx: Sender<HarnessEvent>,
        /// Exact connection generation identity associated with this writer.
        connection_id: tau_proto::ConnectionId,
    },
}

/// Connection sink that admits payloads to one shared logical live stream.
pub(crate) struct ChannelSink {
    /// Shared live stream used by every connection in this harness.
    log: Arc<crate::event_log::EventLog>,
    /// Runtime-only consumer generation.
    consumer: tau_core::SharedConsumerId,
    /// Shared-stream identity used by the route planner.
    group: tau_core::SharedDeliveryGroup,
    /// Whether explicit lifecycle retirement already released this cursor.
    retired: bool,
}

/// Cloneable cursor barrier and lag view retained by harness lifecycle code.
#[derive(Clone)]
pub(crate) struct LiveConsumerHandle {
    /// Shared logical live stream.
    log: Arc<crate::event_log::EventLog>,
    /// Runtime-only consumer generation.
    consumer: tau_core::SharedConsumerId,
}

impl LiveConsumerHandle {
    /// Waits until this generation reaches the stream's current tail.
    pub(crate) fn flush(&self) {
        self.log.flush_consumer(self.consumer);
    }

    /// Requests retirement after every frame admitted through the current tail.
    pub(crate) fn close_after_current(&self) {
        self.log.close_consumer_after_current(self.consumer);
    }

    /// Waits for bounded retirement after a close-after-current request.
    pub(crate) fn wait_for_retirement(&self, timeout: Duration) -> bool {
        self.log
            .wait_for_consumer_retirement(self.consumer, timeout)
    }
}

impl ChannelSink {
    /// Registers a fresh cursor and switches the writer to follower mode.
    pub(crate) fn new(
        tx: &Sender<WriterCommand>,
        log: Arc<crate::event_log::EventLog>,
        failure_tx: Sender<HarnessEvent>,
        connection_id: tau_proto::ConnectionId,
    ) -> Result<Self, ConnectionSendError> {
        let consumer = log.register_consumer();
        if tx
            .send(WriterCommand::Follow {
                log: Arc::clone(&log),
                consumer,
                failure_tx,
                connection_id,
            })
            .is_err()
        {
            log.retire_consumer(consumer);
            return Err(ConnectionSendError::new("writer closed"));
        }
        let group = log.group();
        Ok(Self {
            log,
            consumer,
            group,
            retired: false,
        })
    }

    /// Returns a lifecycle handle for cursor barriers and lag diagnostics.
    pub(crate) fn handle(&self) -> LiveConsumerHandle {
        LiveConsumerHandle {
            log: Arc::clone(&self.log),
            consumer: self.consumer,
        }
    }
}

impl ConnectionSink for ChannelSink {
    fn send(&mut self, routed: tau_core::RoutedFrame) -> Result<(), ConnectionSendError> {
        let admitted = self.log.append_egress(
            routed,
            &[tau_core::SharedDeliveryTarget::new(
                self.group,
                self.consumer,
            )],
        );
        if admitted.is_empty() {
            Err(ConnectionSendError::new("consumer generation retired"))
        } else {
            Ok(())
        }
    }

    fn shared_delivery_target(&self) -> Option<tau_core::SharedDeliveryTarget> {
        Some(tau_core::SharedDeliveryTarget::new(
            self.group,
            self.consumer,
        ))
    }

    fn send_shared(
        &mut self,
        frame: tau_core::RoutedFrame,
        targets: &[tau_core::SharedDeliveryTarget],
    ) -> Result<Vec<tau_core::SharedDeliveryTarget>, ConnectionSendError> {
        Ok(self.log.append_egress(frame, targets))
    }

    fn begin_catch_up(&mut self) {
        self.log.set_catch_up_paused(self.consumer, true);
    }

    fn finish_catch_up(&mut self) {
        self.log.set_catch_up_paused(self.consumer, false);
    }

    fn retire(&mut self) {
        self.log.retire_consumer(self.consumer);
        self.retired = true;
    }
}

impl Drop for ChannelSink {
    fn drop(&mut self) {
        if !self.retired {
            self.log.retire_consumer(self.consumer);
        }
    }
}

/// Non-queueing synchronous sink for internal summarized observations.
pub(crate) struct SynchronousSink {
    /// Callback that consumes each routed payload before `send` returns.
    observe: Box<dyn FnMut(tau_core::RoutedFrame)>,
}

impl SynchronousSink {
    /// Creates a sink around one non-retaining synchronous callback.
    pub(crate) fn new(observe: impl FnMut(tau_core::RoutedFrame) + 'static) -> Self {
        Self {
            observe: Box::new(observe),
        }
    }
}

impl ConnectionSink for SynchronousSink {
    fn send(&mut self, routed: tau_core::RoutedFrame) -> Result<(), ConnectionSendError> {
        (self.observe)(routed);
        Ok(())
    }
}

/// Reader thread — one per connection, sends to the shared harness channel.
pub(crate) fn spawn_reader_thread(
    connection_id: tau_proto::ConnectionId,
    stream: impl io::Read + Send + 'static,
    tx: ComponentIngressSender,
) {
    spawn_reader_thread_inner(connection_id, stream, tx, None);
}

/// Reader thread for extensions whose messages must not enter the harness loop
/// until the harness has created all matching connection and lifecycle state.
pub(crate) fn spawn_reader_thread_after_initialized(
    connection_id: tau_proto::ConnectionId,
    stream: impl io::Read + Send + 'static,
    tx: ComponentIngressSender,
    initialized_rx: Receiver<()>,
) {
    spawn_reader_thread_inner(connection_id, stream, tx, Some(initialized_rx));
}

fn spawn_reader_thread_inner(
    connection_id: tau_proto::ConnectionId,
    stream: impl io::Read + Send + 'static,
    tx: ComponentIngressSender,
    initialized_rx: Option<Receiver<()>>,
) {
    thread::spawn(move || {
        if let Some(initialized_rx) = initialized_rx
            && initialized_rx.recv().is_err()
        {
            return;
        }

        let mut reader = HarnessInputReader::new(stream);
        loop {
            let read_started = Instant::now();
            match reader.read_message_with_size() {
                Ok(Some(decoded)) => {
                    let traffic_class = prompt_traffic_class_for_message(&decoded.message);
                    if let Some(traffic_class) = traffic_class {
                        tracing::trace!(
                            target: "tau_harness::prompt_ingress",
                            stage = "blocking_read_decode",
                            traffic_class,
                            encoded_bytes = decoded.encoded_bytes.get(),
                            socket_wait_read_decode_us = read_started.elapsed().as_micros(),
                            "content-free component ingress stage"
                        );
                    }
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
    CloseStream {
        /// Whether downlink failure immediately fails the whole connection.
        report_failure: bool,
    },
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
    /// Mount staging and mask paths that remain mounted by the supervised
    /// child.
    ///
    /// The child namespace references these paths as bind-mount sources, so
    /// their parent must retain them until the child is reaped.
    _isolation_tempdir: Option<tempfile::TempDir>,
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
    let (tx, _) = spawn_writer_thread_inner(
        writer,
        WriterShutdown::CloseStream {
            report_failure: true,
        },
        protocol_io,
    );
    tx
}

/// Spawns a local UI writer whose independent ingress reader owns disconnect
/// ordering and may still hold a detach request after downlink failure.
pub(crate) fn spawn_initial_stdio_writer_thread(
    writer: impl Write + Send + 'static,
    protocol_io: Option<ProtocolIoMeter>,
) -> Sender<WriterCommand> {
    let (tx, _) = spawn_writer_thread_inner(
        writer,
        WriterShutdown::CloseStream {
            report_failure: false,
        },
        protocol_io,
    );
    tx
}

/// Spawn a supervised writer whose join handle remains owned by the harness.
#[cfg(test)]
pub(crate) fn spawn_supervised_writer_thread(
    connection_id: tau_proto::ConnectionId,
    writer: impl Write + Send + 'static,
    child: Child,
    protocol_io: Option<ProtocolIoMeter>,
    harness_tx: Sender<HarnessEvent>,
) -> (Sender<WriterCommand>, SupervisedWriterHandle) {
    spawn_supervised_writer_thread_with_isolation_tempdir(
        connection_id,
        writer,
        child,
        protocol_io,
        harness_tx,
        None,
    )
}

/// Like [`spawn_supervised_writer_thread`], retaining a child namespace's
/// staging root until the direct child has been reaped.
pub(crate) fn spawn_supervised_writer_thread_with_isolation_tempdir(
    connection_id: tau_proto::ConnectionId,
    writer: impl Write + Send + 'static,
    child: Child,
    protocol_io: Option<ProtocolIoMeter>,
    harness_tx: Sender<HarnessEvent>,
    isolation_tempdir: Option<tempfile::TempDir>,
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
            _isolation_tempdir: isolation_tempdir,
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
        #[cfg_attr(
            not(test),
            expect(
                clippy::never_loop,
                reason = "production receives one permanent Follow command; tests also exercise legacy messages"
            )
        )]
        while let Ok(command) = rx.recv() {
            match command {
                #[cfg(test)]
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
                #[cfg(test)]
                WriterCommand::Flush(ack) => {
                    let _ = w.flush();
                    let _ = ack.send(());
                }
                WriterCommand::Follow {
                    log,
                    consumer,
                    failure_tx,
                    connection_id,
                } => {
                    let mut writer_failed = false;
                    while let Some(pending) = log.next_egress(consumer) {
                        let message = pending.frame();
                        let Ok(frame_bytes) = w.write_message_with_size(message) else {
                            can_write_disconnect = false;
                            writer_failed = true;
                            break;
                        };
                        if w.flush().is_err() {
                            can_write_disconnect = false;
                            writer_failed = true;
                            break;
                        }
                        if let Some(protocol_io) = &protocol_io {
                            protocol_io.record_downlink_frame_bytes(message, frame_bytes);
                        }
                        log.acknowledge_egress(consumer, &pending);
                    }
                    log.retire_consumer_after_io(consumer);
                    // A local UI's ingress reader remains authoritative for
                    // disconnect ordering: it may already hold a detach request
                    // ahead of EOF while the downlink fails concurrently.
                    // Supervised extensions retain writer-failure reporting
                    // because their owned-child lifecycle has no independent
                    // local-UI detach transition to preserve.
                    let report_failure = match &shutdown {
                        WriterShutdown::CloseStream { report_failure } => *report_failure,
                        WriterShutdown::Supervised { .. } => true,
                    };
                    if writer_failed && report_failure {
                        let _ = failure_tx.send(HarnessEvent::ReadFailed {
                            connection_id,
                            error: "connection writer failed".to_owned(),
                        });
                    }
                    break;
                }
            }
        }

        // Channel closed or writer failed — run shutdown sequence.
        match shutdown {
            WriterShutdown::CloseStream { .. } => {
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

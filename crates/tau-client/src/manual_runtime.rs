use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::builder::ExtensionBuilder;
use crate::runner::{dispatch_message, write_hello, write_ready, write_startup};
use crate::writer_thread::{WriterCommand, run_writer};
use crate::{ClientError, ClientHandle, ClientResult, TauExtension};

/// Runtime for extensions that need to drive their own harness receive loop.
///
/// `ManualExtensionRuntime` owns the usual tau-client startup declaration,
/// dispatch table, outbound writer thread, and extension state, but it does not
/// own the policy loop. Callers receive harness frames with [`Self::recv`] or
/// [`Self::recv_timeout`], or combine [`Self::try_recv`] with
/// [`Self::wait_for_wake`] for reactive side-channel work, and pass received
/// messages to [`Self::dispatch_one`].
pub struct ManualExtensionRuntime<State> {
    /// Extension state mutated by dispatch handlers and caller-owned timer
    /// work.
    state: State,
    /// Builder data retained after startup for dispatch handlers.
    builder: ExtensionBuilder<State>,
    /// Cloneable outbound handle for caller-owned side work.
    handle: ClientHandle,
    /// Shared input queue used by manual receive and correlated RPC helpers.
    input: Rc<RefCell<ManualInput>>,
    /// Blocking notification channel signaled by reader and caller-owned work.
    wake_receiver: tau_blocking_notify_channel::Receiver,
    /// Cloneable handle for waking manual runtime loops from caller-owned work.
    waker: ManualRuntimeWaker,
    /// Reader thread that decodes harness messages for manual receive APIs.
    reader_thread: JoinHandle<()>,
    /// Writer thread that serializes outbound protocol frames.
    writer_thread: JoinHandle<ClientResult<()>>,
    /// Whether startup has completed through the terminal `Ready` frame.
    startup: ManualStartupState,
}

/// Shared manual-loop input queue used by receive calls and correlated helpers.
struct ManualInput {
    /// Reader-thread channel carrying decoded harness messages.
    receiver: mpsc::Receiver<ReaderMessage>,
    /// Harness messages read while waiting for a correlated helper response.
    pending: VecDeque<ReaderMessage>,
    /// True once the reader reported EOF or a decode error.
    input_closed: bool,
}

/// Result of polling the manual runtime input side.
#[derive(Debug)]
pub enum ManualRuntimeInput {
    /// A decoded harness-to-peer protocol message is ready for dispatch.
    Message(tau_proto::HarnessOutputMessage),
    /// No input arrived before the requested timeout elapsed.
    Timeout,
    /// The harness input stream ended cleanly at a message boundary.
    InputClosed,
}

/// Result of non-blocking manual runtime input polling.
#[derive(Debug)]
pub enum ManualRuntimePoll {
    /// A decoded harness-to-peer protocol message is ready to handle.
    Message(tau_proto::HarnessOutputMessage),
    /// The harness input stream ended cleanly at a message boundary.
    InputClosed,
    /// No harness input is currently ready.
    Empty,
}

/// Result of dispatching one harness-to-peer protocol message.
#[derive(Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    /// Dispatch completed and the caller should continue its loop.
    Continue,
    /// The harness explicitly requested disconnect.
    Disconnect(tau_proto::Disconnect),
    /// A tool handler requested that the extension stop after this message.
    StopRequested,
}

enum ReaderMessage {
    /// One decoded harness output message.
    Message(tau_proto::HarnessOutputMessage),
    /// Clean EOF at a message boundary.
    InputClosed,
    /// Decode or read failure from the reader thread.
    Error(ClientError),
}

/// Cloneable wake handle for caller-owned manual runtime work.
///
/// A `ManualRuntimeWaker` can be moved to worker threads that have their own
/// side-channel back to the extension loop. Calling [`Self::wake`] makes
/// [`ManualExtensionRuntime::wait_for_wake`] return promptly, allowing the
/// caller-owned single-threaded policy loop to react without polling.
#[derive(Clone, Debug)]
pub struct ManualRuntimeWaker {
    /// Shared coalescing notification sender.
    sender: tau_blocking_notify_channel::Sender,
}

/// Startup phase tracked by manual runtimes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManualStartupState {
    /// The runtime has written only `Hello`; the caller must send dynamic
    /// startup frames and exactly one `Ready` before entering steady-state
    /// dispatch.
    Deferred,
    /// Startup has completed through `Ready`.
    Complete,
}

/// Correlated extension-data RPC helper for manual-loop extensions.
///
/// This client is caller-thread-local (`Rc`-backed, not `Send`). It shares the
/// manual runtime input queue so handler-owned storage operations can wait for
/// matching `ExtensionDataResult` frames without directly reading the transport
/// or stealing unrelated frames from the outer manual loop.
#[derive(Clone)]
pub struct ExtensionDataClient {
    /// Outbound handle used to send extension-data request frames.
    handle: ClientHandle,
    /// Shared input queue used to wait for correlated response frames.
    input: Rc<RefCell<ManualInput>>,
}

/// Error returned by manual-loop extension-data RPC helpers.
#[derive(Debug)]
pub enum ExtensionDataRpcError {
    /// Sending the request or reading the input stream failed.
    Client(ClientError),
    /// Harness returned an extension-data operation error.
    Harness {
        /// Machine-readable error kind reported by the harness.
        kind: tau_proto::ExtensionDataErrorKind,
        /// Human-readable error details reported by the harness.
        message: String,
    },
    /// The timeout elapsed before the matching response arrived.
    Timeout,
    /// The harness input stream reached clean EOF before the matching response.
    InputClosed,
    /// The harness explicitly disconnected before the matching response.
    Disconnect(tau_proto::Disconnect),
}

impl fmt::Display for ExtensionDataRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(f, "{error}"),
            Self::Harness { kind, message } => write!(f, "{kind:?}: {message}"),
            Self::Timeout => f.write_str("extension data request timed out"),
            Self::InputClosed => f.write_str("harness input closed during extension data request"),
            Self::Disconnect(disconnect) => match &disconnect.reason {
                Some(reason) => write!(
                    f,
                    "harness disconnected during extension data request: {reason}"
                ),
                None => f.write_str("harness disconnected during extension data request"),
            },
        }
    }
}

impl std::error::Error for ExtensionDataRpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::Harness { .. } | Self::Timeout | Self::InputClosed | Self::Disconnect(_) => None,
        }
    }
}

impl From<ClientError> for ExtensionDataRpcError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

static NEXT_EXTENSION_DATA_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

impl ExtensionDataClient {
    /// Sends one extension-data request and waits for its correlated response.
    ///
    /// This call has no timeout and may wait indefinitely if the harness never
    /// sends a matching `ExtensionDataResult`.
    ///
    /// Unrelated harness frames received while waiting are preserved and
    /// returned by later manual-runtime receive calls in their original order,
    /// except that protocol `Disconnect` is treated as a priority shutdown
    /// control frame. If a `Disconnect` arrives before the matching result, it
    /// is both returned as [`ExtensionDataRpcError::Disconnect`] and re-queued
    /// ahead of deferred side-effect frames for the outer manual loop to
    /// observe.
    ///
    /// # Errors
    ///
    /// Returns an error when output fails, the harness returns an
    /// extension-data operation error, the input stream closes, or a protocol
    /// `Disconnect` arrives before the matching result.
    pub fn request(
        &self,
        scope: tau_proto::ExtensionDataScope,
        op: tau_proto::ExtensionDataRequestOp,
    ) -> Result<tau_proto::ExtensionDataValue, ExtensionDataRpcError> {
        self.request_inner(scope, op, None)
    }

    /// Sends one extension-data request and waits up to `timeout` for its
    /// correlated response.
    ///
    /// Unrelated harness frames and priority protocol `Disconnect` are
    /// preserved as in [`Self::request`]. If the timeout elapses, the request
    /// is not cancelled at the protocol layer; a later result with the same
    /// request id will be treated as an unrelated frame by the manual loop.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionDataRpcError::Timeout`] when no matching result
    /// arrives before `timeout`, or the same errors as [`Self::request`].
    pub fn request_timeout(
        &self,
        scope: tau_proto::ExtensionDataScope,
        op: tau_proto::ExtensionDataRequestOp,
        timeout: Duration,
    ) -> Result<tau_proto::ExtensionDataValue, ExtensionDataRpcError> {
        self.request_inner(scope, op, Some(timeout))
    }

    fn request_inner(
        &self,
        scope: tau_proto::ExtensionDataScope,
        op: tau_proto::ExtensionDataRequestOp,
        timeout: Option<Duration>,
    ) -> Result<tau_proto::ExtensionDataValue, ExtensionDataRpcError> {
        let request_id = format!(
            "tau-client-extension-data-{}",
            NEXT_EXTENSION_DATA_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
        );
        self.handle
            .send(tau_proto::HarnessInputMessage::ExtensionDataRequest(
                tau_proto::ExtensionDataRequest {
                    request_id: request_id.clone(),
                    scope,
                    op,
                },
            ))?;
        self.input
            .borrow_mut()
            .wait_for_extension_data_result(request_id, timeout)
    }
}

impl<State> ManualExtensionRuntime<State> {
    /// Returns a shared reference to caller-owned extension state.
    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Returns a mutable reference to caller-owned extension state.
    #[must_use]
    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    /// Returns a cloneable handle for outbound frames.
    #[must_use]
    pub fn handle(&self) -> ClientHandle {
        self.handle.clone()
    }

    /// Returns a caller-thread-local extension-data RPC helper.
    #[must_use]
    pub fn extension_data_client(&self) -> ExtensionDataClient {
        ExtensionDataClient {
            handle: self.handle.clone(),
            input: Rc::clone(&self.input),
        }
    }

    /// Returns a cloneable handle that wakes this manual runtime's policy loop.
    #[must_use]
    pub fn waker(&self) -> ManualRuntimeWaker {
        self.waker.clone()
    }

    /// Sends a dynamic startup `Subscribe` frame before `Ready`.
    ///
    /// This is available only for runtimes started with
    /// [`crate::TauExtensionRunner::start_manual_loop_deferred_startup`]. It
    /// lets config-gated extensions compute their subscription set after
    /// reading initial harness configuration while preserving normal
    /// startup staging: `Hello`, zero or more dynamic startup frames, then
    /// exactly one `Ready`.
    ///
    /// # Errors
    ///
    /// Returns an error when startup has already completed or the frame cannot
    /// be queued and flushed.
    pub fn startup_subscribe(
        &mut self,
        selectors: impl IntoIterator<Item = tau_proto::EventSelector>,
    ) -> ClientResult<()> {
        self.ensure_deferred_startup()?;
        let selectors: Vec<_> = selectors.into_iter().collect();
        self.handle.send(tau_proto::HarnessInputMessage::Subscribe(
            tau_proto::Subscribe {
                historical_selectors: Vec::new(),
                live_selectors: selectors,
            },
        ))
    }

    /// Sends a dynamic startup `Subscribe` frame with explicit restore/live
    /// sets.
    pub fn startup_subscribe_split(
        &mut self,
        historical_selectors: impl IntoIterator<Item = tau_proto::EventSelector>,
        live_selectors: impl IntoIterator<Item = tau_proto::EventSelector>,
    ) -> ClientResult<()> {
        self.ensure_deferred_startup()?;
        self.handle.send(tau_proto::HarnessInputMessage::Subscribe(
            tau_proto::Subscribe {
                historical_selectors: historical_selectors.into_iter().collect(),
                live_selectors: live_selectors.into_iter().collect(),
            },
        ))
    }

    /// Sends a dynamic startup `Intercept` frame before `Ready`.
    ///
    /// Use this for extensions whose interception selectors are known only
    /// after config-gated initialization. The runtime does not install an
    /// intercept handler; callers remain responsible for replying to each
    /// later `InterceptRequest`, for example with
    /// [`ClientHandle::intercept_reply`]. Dynamic intercept registration should
    /// normally be sent once with the complete selector set because harness
    /// interceptor registration is a connection-level declaration rather than
    /// an additive list of independent handlers.
    ///
    /// # Errors
    ///
    /// Returns an error when startup has already completed or the frame cannot
    /// be queued and flushed.
    pub fn startup_intercept(
        &mut self,
        selectors: impl IntoIterator<Item = tau_proto::EventSelector>,
        priority: tau_proto::InterceptionPriority,
    ) -> ClientResult<()> {
        self.ensure_deferred_startup()?;
        self.handle.send(tau_proto::HarnessInputMessage::Intercept(
            tau_proto::Intercept {
                selectors: selectors.into_iter().collect(),
                priority,
            },
        ))
    }

    /// Emits one dynamic startup event before `Ready`.
    ///
    /// This is intended for declarations such as tool registrations or action
    /// schemas that are computed after initial configuration. Runtime events
    /// after `Ready` should use [`Self::handle`] and [`ClientHandle::emit`]
    /// instead.
    ///
    /// # Errors
    ///
    /// Returns an error when startup has already completed or the event cannot
    /// be queued and flushed.
    pub fn startup_event(&mut self, event: tau_proto::Event) -> ClientResult<()> {
        self.ensure_deferred_startup()?;
        self.handle.emit(event)
    }

    /// Sends the terminal startup `Ready` frame for a deferred manual runtime.
    ///
    /// After this succeeds, subsequent dynamic startup helpers return an error.
    /// Callers should then enter their normal manual receive loop with
    /// [`Self::recv`] / [`Self::recv_timeout`] or [`Self::try_recv`] /
    /// [`Self::wait_for_wake`], and [`Self::dispatch_one`].
    ///
    /// # Errors
    ///
    /// Returns an error when startup already completed or the `Ready` frame
    /// cannot be queued and flushed.
    pub fn startup_ready(&mut self, message: Option<String>) -> ClientResult<()> {
        self.ensure_deferred_startup()?;
        write_ready(&self.handle, message)?;
        self.startup = ManualStartupState::Complete;
        Ok(())
    }

    /// Waits until the next harness message or input EOF is available.
    ///
    /// After this method returns [`ManualRuntimeInput::InputClosed`],
    /// subsequent receive calls also return `InputClosed` immediately. The
    /// caller may still emit outbound frames through [`Self::handle`] and
    /// later call [`Self::finish`] to flush them.
    ///
    /// # Errors
    ///
    /// Returns an error when the reader thread reports a protocol decode/read
    /// failure or exits without sending its terminal status.
    pub fn recv(&mut self) -> ClientResult<ManualRuntimeInput> {
        self.input.borrow_mut().recv()
    }

    /// Waits up to `timeout` for the next harness message or input EOF.
    ///
    /// `Timeout` is distinct from both clean input EOF and protocol
    /// `Disconnect`, allowing caller-owned loops to drive timers without losing
    /// tau-client dispatch semantics.
    ///
    /// # Errors
    ///
    /// Returns an error when the reader thread reports a protocol decode/read
    /// failure or exits without sending its terminal status.
    pub fn recv_timeout(&mut self, timeout: Duration) -> ClientResult<ManualRuntimeInput> {
        self.input.borrow_mut().recv_timeout(timeout)
    }

    /// Attempts to receive a harness message or input EOF without blocking.
    ///
    /// This is intended for event-driven loops that combine harness input with
    /// a caller-owned side channel. Such loops should drain `try_recv` until
    /// [`ManualRuntimePoll::Empty`], drain their side channel, and then block
    /// in [`Self::wait_for_wake`]. The runtime's internal reader thread and
    /// any clones of [`ManualRuntimeWaker`] signal the same coalesced wake
    /// channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the reader thread reports a protocol decode/read
    /// failure or exits without sending its terminal status.
    pub fn try_recv(&mut self) -> ClientResult<ManualRuntimePoll> {
        self.input.borrow_mut().try_recv()
    }

    /// Blocks until harness input or caller-owned work signals this runtime.
    ///
    /// Notifications are coalesced, so callers must always drain all currently
    /// ready sources after this method returns. If every wake sender has been
    /// dropped, the method returns; a subsequent drain will observe any
    /// terminal input state that remains.
    pub fn wait_for_wake(&self) {
        let _ = self.wake_receiver.recv();
    }

    /// Sends one extension-data request and waits for its correlated response.
    ///
    /// This call has no timeout and may wait indefinitely if the harness never
    /// sends a matching `ExtensionDataResult`.
    ///
    /// Unrelated harness frames received while waiting are preserved and
    /// returned by later [`Self::recv`] / [`Self::recv_timeout`] calls in their
    /// original order, except that protocol `Disconnect` is treated as a
    /// priority shutdown control frame. If a `Disconnect` arrives before the
    /// matching result, it is both returned as
    /// [`ExtensionDataRpcError::Disconnect`] and re-queued ahead of deferred
    /// side-effect frames for the outer manual loop to observe. This lets
    /// extensions perform synchronous storage operations from a manual loop
    /// without directly reading the transport or stealing unrelated messages
    /// from tau-client dispatch.
    ///
    /// This helper is available only on manual extension runtimes, and sends
    /// the existing protocol
    /// [`tau_proto::HarnessInputMessage::ExtensionDataRequest`]
    /// frame without broadening peer capabilities.
    ///
    /// # Errors
    ///
    /// Returns an error when output fails, the harness returns an
    /// extension-data operation error, the input stream closes, or a protocol
    /// `Disconnect` arrives before the matching result.
    pub fn extension_data_request(
        &mut self,
        scope: tau_proto::ExtensionDataScope,
        op: tau_proto::ExtensionDataRequestOp,
    ) -> Result<tau_proto::ExtensionDataValue, ExtensionDataRpcError> {
        self.extension_data_client().request(scope, op)
    }

    /// Sends one extension-data request and waits up to `timeout` for its
    /// correlated response.
    ///
    /// Unrelated frames and priority protocol `Disconnect` are preserved as in
    /// [`Self::extension_data_request`]. If the timeout elapses, the request is
    /// not cancelled at the protocol layer; a later result with the same
    /// request id will be treated as an unrelated frame by the manual loop.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionDataRpcError::Timeout`] when no matching result
    /// arrives before `timeout`, or the same errors as
    /// [`Self::extension_data_request`].
    pub fn extension_data_request_timeout(
        &mut self,
        scope: tau_proto::ExtensionDataScope,
        op: tau_proto::ExtensionDataRequestOp,
        timeout: Duration,
    ) -> Result<tau_proto::ExtensionDataValue, ExtensionDataRpcError> {
        self.extension_data_client()
            .request_timeout(scope, op, timeout)
    }

    /// Dispatches one harness message through the registered tau-client
    /// handlers.
    ///
    /// This preserves the same semantics as [`crate::TauExtensionRunner::run`]:
    /// configuration errors are emitted and the loop normally continues,
    /// live-only handlers skip replay-marked deliveries, tool stop requests are
    /// reported to the caller, and intercept handler failures still send one
    /// pass-through reply before surfacing the error.
    ///
    /// # Errors
    ///
    /// Returns an error when a dispatch handler requests a fatal stop, when
    /// intercept reply output fails, or when outbound config/error frames
    /// cannot be queued.
    pub fn dispatch_one(
        &mut self,
        message: tau_proto::HarnessOutputMessage,
    ) -> ClientResult<DispatchOutcome> {
        dispatch_message(message, &mut self.state, &mut self.builder, &self.handle)
    }

    /// Gracefully shuts down the writer, waits for observable threads to
    /// finish, and returns extension state.
    ///
    /// Use this after clean input EOF, caller-requested stop, or other paths
    /// where queued output should be flushed and writer errors should be
    /// reported. If background workers may still hold handles and may be
    /// blocked on output after a harness `Disconnect`, use
    /// [`Self::finish_detached`] instead.
    ///
    /// Arbitrary [`Read`] implementations cannot be cancelled portably. When
    /// the input side has already reached EOF or the reader thread has
    /// otherwise finished, this method joins the reader and reports a panic
    /// as [`ClientError::ReaderPanicked`]. When input is still open, this
    /// method detaches the blocked reader thread; dropping the runtime
    /// closes the receive channel, so the reader exits after its next read
    /// completes.
    ///
    /// # Errors
    ///
    /// Returns an error when reader completion reports a panic, writer shutdown
    /// cannot be queued, the writer thread panics, or the writer reports an
    /// encode/flush error.
    pub fn finish(self) -> ClientResult<State> {
        let reader_message_result = if self.reader_thread.is_finished() {
            self.input.borrow_mut().drain_finished_reader_status()
        } else {
            Ok(())
        };
        let input_closed = self.input.borrow().input_closed;
        let reader_join_result = if input_closed || self.reader_thread.is_finished() {
            self.reader_thread
                .join()
                .map_err(|_| ClientError::ReaderPanicked)
        } else {
            Ok(())
        };
        let shutdown_result = self.handle.shutdown();
        let writer_result = self
            .writer_thread
            .join()
            .map_err(|_| ClientError::WriterPanicked)
            .and_then(|result| result);
        match (
            reader_message_result,
            reader_join_result,
            shutdown_result,
            writer_result,
        ) {
            (Ok(()), Ok(()), Ok(()), Ok(())) => Ok(self.state),
            (Err(error), _, _, _) => Err(error),
            (_, Err(error), _, _) => Err(error),
            (_, _, Err(error), _) => Err(error),
            (_, _, _, Err(error)) => Err(error),
        }
    }

    /// Returns extension state without shutting down or joining background
    /// protocol threads.
    ///
    /// This mirrors the detached-writer disconnect mode used by chat bridge
    /// extensions: after a harness `Disconnect`, callers can keep disconnect
    /// latency independent of queued background output, output backpressure, or
    /// a still-open input stream. Any later output sent through cloned
    /// handles is best-effort and may be dropped by process shutdown.
    #[must_use]
    pub fn finish_detached(self) -> State {
        self.state
    }

    fn ensure_deferred_startup(&self) -> ClientResult<()> {
        match self.startup {
            ManualStartupState::Deferred => Ok(()),
            ManualStartupState::Complete => Err(ClientError::handler(
                "deferred manual startup has already completed",
            )),
        }
    }
}

impl ManualRuntimeWaker {
    /// Wakes a manual runtime loop blocked in
    /// [`ManualExtensionRuntime::wait_for_wake`].
    ///
    /// Wakes are payload-free and coalesced: multiple calls before the runtime
    /// observes the wake may produce only one return from `wait_for_wake`.
    /// Worker threads should make side-channel work observable, such as by
    /// enqueueing it, before calling this method.
    pub fn wake(&self) {
        self.sender.notify();
    }
}

impl ManualInput {
    fn recv(&mut self) -> ClientResult<ManualRuntimeInput> {
        if let Some(message) = self.pending.pop_front() {
            return self.handle_reader_message(message);
        }
        if self.input_closed {
            return Ok(ManualRuntimeInput::InputClosed);
        }
        self.recv_from_reader()
    }

    fn recv_timeout(&mut self, timeout: Duration) -> ClientResult<ManualRuntimeInput> {
        if let Some(message) = self.pending.pop_front() {
            return self.handle_reader_message(message);
        }
        if self.input_closed {
            return Ok(ManualRuntimeInput::InputClosed);
        }
        self.recv_timeout_from_reader(timeout)
    }

    fn try_recv(&mut self) -> ClientResult<ManualRuntimePoll> {
        if let Some(message) = self.pending.pop_front() {
            return self.handle_reader_poll_message(message);
        }
        if self.input_closed {
            return Ok(ManualRuntimePoll::InputClosed);
        }
        match self.receiver.try_recv() {
            Ok(message) => self.handle_reader_poll_message(message),
            Err(mpsc::TryRecvError::Empty) => Ok(ManualRuntimePoll::Empty),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.input_closed = true;
                Err(ClientError::ReaderClosed)
            }
        }
    }

    fn handle_reader_poll_message(
        &mut self,
        message: ReaderMessage,
    ) -> ClientResult<ManualRuntimePoll> {
        match self.handle_reader_message(message)? {
            ManualRuntimeInput::Message(message) => Ok(ManualRuntimePoll::Message(message)),
            ManualRuntimeInput::InputClosed => Ok(ManualRuntimePoll::InputClosed),
            ManualRuntimeInput::Timeout => unreachable!("try_recv does not produce timeouts"),
        }
    }

    fn recv_from_reader(&mut self) -> ClientResult<ManualRuntimeInput> {
        match self.receiver.recv() {
            Ok(message) => self.handle_reader_message(message),
            Err(_) => {
                self.input_closed = true;
                Err(ClientError::ReaderClosed)
            }
        }
    }

    fn recv_timeout_from_reader(&mut self, timeout: Duration) -> ClientResult<ManualRuntimeInput> {
        match self.receiver.recv_timeout(timeout) {
            Ok(message) => self.handle_reader_message(message),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(ManualRuntimeInput::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.input_closed = true;
                Err(ClientError::ReaderClosed)
            }
        }
    }

    fn handle_reader_message(
        &mut self,
        message: ReaderMessage,
    ) -> ClientResult<ManualRuntimeInput> {
        match message {
            ReaderMessage::Message(message) => Ok(ManualRuntimeInput::Message(message)),
            ReaderMessage::InputClosed => {
                self.input_closed = true;
                Ok(ManualRuntimeInput::InputClosed)
            }
            ReaderMessage::Error(error) => {
                self.input_closed = true;
                Err(error)
            }
        }
    }

    fn drain_finished_reader_status(&mut self) -> ClientResult<()> {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                ReaderMessage::Message(_) => {}
                ReaderMessage::InputClosed => {
                    self.input_closed = true;
                }
                ReaderMessage::Error(error) => {
                    self.input_closed = true;
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn wait_for_extension_data_result(
        &mut self,
        request_id: String,
        timeout: Option<Duration>,
    ) -> Result<tau_proto::ExtensionDataValue, ExtensionDataRpcError> {
        let mut deferred = std::mem::take(&mut self.pending);
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let result = loop {
            let input = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break Err(ExtensionDataRpcError::Timeout);
                    }
                    match self.recv_timeout_from_reader(remaining) {
                        Ok(input) => input,
                        Err(error) => break Err(ExtensionDataRpcError::Client(error)),
                    }
                }
                None => match self.recv_from_reader() {
                    Ok(input) => input,
                    Err(error) => break Err(ExtensionDataRpcError::Client(error)),
                },
            };
            match input {
                ManualRuntimeInput::Message(
                    tau_proto::HarnessOutputMessage::ExtensionDataResult(result),
                ) if result.request_id == request_id => {
                    break match result.result {
                        tau_proto::ExtensionDataResultPayload::Ok { value } => Ok(value),
                        tau_proto::ExtensionDataResultPayload::Error { kind, message } => {
                            Err(ExtensionDataRpcError::Harness { kind, message })
                        }
                    };
                }
                ManualRuntimeInput::Message(tau_proto::HarnessOutputMessage::Disconnect(
                    disconnect,
                )) => {
                    deferred.push_front(ReaderMessage::Message(
                        tau_proto::HarnessOutputMessage::Disconnect(disconnect.clone()),
                    ));
                    break Err(ExtensionDataRpcError::Disconnect(disconnect));
                }
                ManualRuntimeInput::Message(message) => {
                    deferred.push_back(ReaderMessage::Message(message));
                }
                ManualRuntimeInput::Timeout => break Err(ExtensionDataRpcError::Timeout),
                ManualRuntimeInput::InputClosed => break Err(ExtensionDataRpcError::InputClosed),
            }
        };
        deferred.append(&mut self.pending);
        self.pending = deferred;
        result
    }
}

impl<Extension> crate::TauExtensionRunner<Extension>
where
    Extension: TauExtension,
{
    /// Starts the extension and returns a runtime for caller-owned receive
    /// loops.
    ///
    /// The startup prelude is written in the same order as [`Self::run`] and
    /// completes through `Ready` before this method returns. Only the reader
    /// and writer move to background threads; extension state and handlers
    /// remain on the caller thread and do not need to be `Send`.
    ///
    /// # Errors
    ///
    /// Returns an error when builder validation fails, startup output cannot be
    /// encoded or flushed.
    pub fn start_manual_loop<R, W>(
        self,
        reader: R,
        writer: W,
        state: Extension::State,
    ) -> ClientResult<ManualExtensionRuntime<Extension::State>>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        self.start_manual_loop_with_state(reader, writer, |_| state)
    }

    /// Starts the extension and constructs state after startup frames are
    /// ready.
    ///
    /// The supplied factory receives a cloneable [`ClientHandle`] only after
    /// the complete startup prelude has been written through `Ready`,
    /// preserving the normal startup staging while allowing caller-owned
    /// loops or background workers to retain outbound handles.
    ///
    /// # Errors
    ///
    /// Returns an error when builder validation fails, startup output cannot be
    /// encoded or flushed.
    pub fn start_manual_loop_with_state<R, W, MakeState>(
        self,
        reader: R,
        writer: W,
        make_state: MakeState,
    ) -> ClientResult<ManualExtensionRuntime<Extension::State>>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
        MakeState: FnOnce(ClientHandle) -> Extension::State,
    {
        let mut builder = ExtensionBuilder::new(self.extension.name(), self.extension.kind());
        self.extension.register(&mut builder);
        builder.validate()?;

        let (sender, receiver) = mpsc::channel::<WriterCommand>();
        let handle = ClientHandle::new(sender);
        let writer_thread = std::thread::spawn(move || run_writer(writer, receiver));

        if let Err(error) = write_startup(&builder, &handle) {
            let _ = handle.shutdown();
            let _ = writer_thread.join();
            return Err(error);
        }

        let state = make_state(handle.clone());
        let (wake_sender, wake_receiver) = tau_blocking_notify_channel::channel();
        let (input, reader_thread) = spawn_reader_thread(reader, wake_sender.clone());
        Ok(ManualExtensionRuntime {
            state,
            builder,
            handle,
            input: Rc::new(RefCell::new(ManualInput {
                receiver: input,
                pending: VecDeque::new(),
                input_closed: false,
            })),
            wake_receiver,
            waker: ManualRuntimeWaker {
                sender: wake_sender,
            },
            reader_thread,
            writer_thread,
            startup: ManualStartupState::Complete,
        })
    }

    /// Starts a manual runtime in config-gated deferred-startup mode.
    ///
    /// This writes and flushes only the initial `Hello` frame, starts the
    /// reader and writer threads, constructs state, and returns before any
    /// `Subscribe`, `Intercept`, startup `Emit`, or `Ready` frames are sent.
    /// Callers can then receive initial configuration, compute dynamic startup
    /// declarations, send them with
    /// [`ManualExtensionRuntime::startup_subscribe`],
    /// [`ManualExtensionRuntime::startup_intercept`], and
    /// [`ManualExtensionRuntime::startup_event`], and finish exactly once with
    /// [`ManualExtensionRuntime::startup_ready`].
    ///
    /// Builders used with this mode may register dispatch handlers, but must
    /// not declare static startup frames such as subscriptions, intercepts,
    /// startup events, tools, actions, or a ready message. Dynamic startup
    /// declarations are intentionally explicit so config-gated extensions
    /// cannot accidentally leak pre-configuration startup state.
    ///
    /// # Errors
    ///
    /// Returns an error when builder validation fails, static startup
    /// declarations are present, or the initial `Hello` frame cannot be encoded
    /// and flushed.
    pub fn start_manual_loop_deferred_startup<R, W>(
        self,
        reader: R,
        writer: W,
        state: Extension::State,
    ) -> ClientResult<ManualExtensionRuntime<Extension::State>>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        self.start_manual_loop_deferred_startup_with_state(reader, writer, |_| state)
    }

    /// Starts a deferred-startup manual runtime and constructs state after
    /// `Hello` is written.
    ///
    /// The supplied factory receives a cloneable [`ClientHandle`] immediately
    /// after `Hello` is flushed, before dynamic startup declarations and
    /// `Ready`. The caller is responsible for preserving the one-way startup
    /// contract documented on [`Self::start_manual_loop_deferred_startup`].
    ///
    /// # Errors
    ///
    /// Returns an error when builder validation fails, static startup
    /// declarations are present, or the initial `Hello` frame cannot be encoded
    /// and flushed.
    pub fn start_manual_loop_deferred_startup_with_state<R, W, MakeState>(
        self,
        reader: R,
        writer: W,
        make_state: MakeState,
    ) -> ClientResult<ManualExtensionRuntime<Extension::State>>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
        MakeState: FnOnce(ClientHandle) -> Extension::State,
    {
        let mut builder = ExtensionBuilder::new(self.extension.name(), self.extension.kind());
        self.extension.register(&mut builder);
        builder.validate()?;
        builder.validate_deferred_startup()?;

        let (sender, receiver) = mpsc::channel::<WriterCommand>();
        let handle = ClientHandle::new(sender);
        let writer_thread = std::thread::spawn(move || run_writer(writer, receiver));

        if let Err(error) = write_hello(&builder, &handle) {
            let _ = handle.shutdown();
            let _ = writer_thread.join();
            return Err(error);
        }

        let state = make_state(handle.clone());
        let (wake_sender, wake_receiver) = tau_blocking_notify_channel::channel();
        let (input, reader_thread) = spawn_reader_thread(reader, wake_sender.clone());
        Ok(ManualExtensionRuntime {
            state,
            builder,
            handle,
            input: Rc::new(RefCell::new(ManualInput {
                receiver: input,
                pending: VecDeque::new(),
                input_closed: false,
            })),
            wake_receiver,
            waker: ManualRuntimeWaker {
                sender: wake_sender,
            },
            reader_thread,
            writer_thread,
            startup: ManualStartupState::Deferred,
        })
    }
}

fn spawn_reader_thread<R>(
    reader: R,
    wake_sender: tau_blocking_notify_channel::Sender,
) -> (mpsc::Receiver<ReaderMessage>, JoinHandle<()>)
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader_thread = std::thread::spawn(move || {
        /// Exit guard that wakes waiters when the reader thread leaves.
        struct WakeOnDrop {
            /// Wake sender shared with manual runtime waiters.
            sender: tau_blocking_notify_channel::Sender,
        }

        impl Drop for WakeOnDrop {
            fn drop(&mut self) {
                self.sender.notify();
            }
        }

        let _wake_on_exit = WakeOnDrop {
            sender: wake_sender.clone(),
        };
        let mut reader = tau_proto::PeerInputReader::new(reader);
        loop {
            let message = match reader.read_message() {
                Ok(Some(message)) => ReaderMessage::Message(message),
                Ok(None) => ReaderMessage::InputClosed,
                Err(error) => ReaderMessage::Error(ClientError::from(error)),
            };
            let should_stop = !matches!(message, ReaderMessage::Message(_));
            if sender.send(message).is_err() {
                break;
            }
            wake_sender.notify();
            if should_stop {
                break;
            }
        }
    });
    (receiver, reader_thread)
}

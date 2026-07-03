use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::builder::ExtensionBuilder;
use crate::runner::{dispatch_message, write_startup};
use crate::writer_thread::{WriterCommand, run_writer};
use crate::{ClientError, ClientHandle, ClientResult, TauExtension};

/// Runtime for extensions that need to drive their own harness receive loop.
///
/// `ManualExtensionRuntime` owns the usual tau-client startup declaration,
/// dispatch table, outbound writer thread, and extension state, but it does not
/// own the policy loop. Callers receive harness frames with [`Self::recv`] or
/// [`Self::recv_timeout`], interleave their own timer or side-effect work, and
/// pass received messages to [`Self::dispatch_one`].
pub struct ManualExtensionRuntime<State> {
    /// Extension state mutated by dispatch handlers and caller-owned timer
    /// work.
    state: State,
    /// Builder data retained after startup for dispatch handlers.
    builder: ExtensionBuilder<State>,
    /// Cloneable outbound handle for caller-owned side work.
    handle: ClientHandle,
    /// Reader-thread channel carrying decoded harness messages.
    input: mpsc::Receiver<ReaderMessage>,
    /// Reader thread that decodes harness messages for timeout-based receive.
    reader_thread: JoinHandle<()>,
    /// Writer thread that serializes outbound protocol frames.
    writer_thread: JoinHandle<ClientResult<()>>,
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
        if self.input_closed {
            return Ok(ManualRuntimeInput::InputClosed);
        }
        match self.input.recv() {
            Ok(message) => self.handle_reader_message(message),
            Err(_) => {
                self.input_closed = true;
                Err(ClientError::ReaderClosed)
            }
        }
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
        if self.input_closed {
            return Ok(ManualRuntimeInput::InputClosed);
        }
        match self.input.recv_timeout(timeout) {
            Ok(message) => self.handle_reader_message(message),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(ManualRuntimeInput::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.input_closed = true;
                Err(ClientError::ReaderClosed)
            }
        }
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
    pub fn finish(mut self) -> ClientResult<State> {
        let reader_message_result = if self.reader_thread.is_finished() {
            self.drain_finished_reader_status()
        } else {
            Ok(())
        };
        let reader_join_result = if self.input_closed || self.reader_thread.is_finished() {
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
        while let Ok(message) = self.input.try_recv() {
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
        let (input, reader_thread) = spawn_reader_thread(reader);
        Ok(ManualExtensionRuntime {
            state,
            builder,
            handle,
            input,
            reader_thread,
            writer_thread,
            input_closed: false,
        })
    }
}

fn spawn_reader_thread<R>(reader: R) -> (mpsc::Receiver<ReaderMessage>, JoinHandle<()>)
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader_thread = std::thread::spawn(move || {
        let mut reader = tau_proto::PeerInputReader::new(reader);
        loop {
            let message = match reader.read_message() {
                Ok(Some(message)) => ReaderMessage::Message(message),
                Ok(None) => ReaderMessage::InputClosed,
                Err(error) => ReaderMessage::Error(ClientError::from(error)),
            };
            let should_stop = !matches!(message, ReaderMessage::Message(_));
            if sender.send(message).is_err() || should_stop {
                break;
            }
        }
    });
    (receiver, reader_thread)
}

use std::sync::mpsc;

use crate::writer_thread::WriterCommand;
use crate::{ClientError, ClientResult};

/// Cloneable outbound handle for sending peer-to-harness protocol frames.
#[derive(Clone)]
pub struct ClientHandle {
    /// Channel to the serialized writer thread.
    sender: mpsc::Sender<WriterCommand>,
}

impl ClientHandle {
    /// Creates a handle around a writer command channel.
    #[must_use]
    pub(crate) fn new(sender: mpsc::Sender<WriterCommand>) -> Self {
        Self { sender }
    }

    /// Sends one raw peer-to-harness message and waits until it is flushed.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer thread has stopped, the frame cannot be
    /// encoded or flushed, or the writer reports an I/O failure.
    pub fn send(&self, message: tau_proto::HarnessInputMessage) -> ClientResult<()> {
        let (ack_sender, ack_receiver) = mpsc::channel();
        self.sender
            .send(WriterCommand::Send(message, ack_sender))
            .map_err(|_| ClientError::WriterClosed)?;
        ack_receiver.recv().map_err(|_| ClientError::WriterClosed)?
    }

    /// Enqueues one peer-to-harness message without waiting for it to flush.
    ///
    /// This is intended for detached background workers whose result should not
    /// block the protocol reader. Use [`Self::send`] when the caller must know
    /// whether the frame was encoded and flushed before it continues.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn send_detached(&self, message: tau_proto::HarnessInputMessage) -> ClientResult<()> {
        self.sender
            .send(WriterCommand::SendDetached(message))
            .map_err(|_| ClientError::WriterClosed)
    }

    /// Emits a durable event through the harness.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn emit(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::emit(event))
    }

    /// Enqueues a durable event through the harness without waiting for flush.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn emit_detached(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.send_detached(tau_proto::HarnessInputMessage::emit(event))
    }

    /// Emits an event with transient delivery metadata through the harness.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn emit_transient(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::emit_with_transient(
            event, true,
        ))
    }

    /// Enqueues a transient event through the harness without waiting for
    /// flush.
    ///
    /// # Errors
    ///
    /// Returns an error only when the writer thread has already stopped before
    /// the frame can be queued.
    pub fn emit_transient_detached(&self, event: tau_proto::Event) -> ClientResult<()> {
        self.send_detached(tau_proto::HarnessInputMessage::emit_with_transient(
            event, true,
        ))
    }

    /// Reports an extension configuration failure to the harness.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn config_error(&self, message: impl Into<String>) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::ConfigError(
            tau_proto::ConfigError {
                message: message.into(),
            },
        ))
    }

    /// Requests a clean peer disconnect with a reason string.
    ///
    /// # Errors
    ///
    /// Returns an error when sending the underlying protocol frame fails.
    pub fn disconnect(&self, reason: impl Into<String>) -> ClientResult<()> {
        self.send(tau_proto::HarnessInputMessage::Disconnect(
            tau_proto::Disconnect {
                reason: Some(reason.into()),
            },
        ))
    }

    /// Stops the writer thread after flushing any pending state.
    pub(crate) fn shutdown(&self) -> ClientResult<()> {
        let (ack_sender, ack_receiver) = mpsc::channel();
        self.sender
            .send(WriterCommand::Shutdown(ack_sender))
            .map_err(|_| ClientError::WriterClosed)?;
        ack_receiver.recv().map_err(|_| ClientError::WriterClosed)?
    }
}

use std::io::Write;
use std::sync::{Arc, mpsc};

use crate::detached_output::{DetachedOutput, QueuedFrame};
use crate::{ClientError, ClientResult};

/// Number of commands allowed to wait outside the active transport write.
pub(crate) const WRITER_QUEUE_ITEMS: usize = 1;

/// Creates the deliberately small, backpressured writer command lane.
pub(crate) fn writer_channel() -> (WriterSender, WriterReceiver) {
    let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_ITEMS);
    let detached = Arc::new(DetachedOutput::new());
    (
        WriterSender {
            commands: sender,
            detached: Arc::clone(&detached),
        },
        WriterReceiver {
            commands: receiver,
            detached,
        },
    )
}

/// Sending half of the writer transport and its shared detached FIFO.
#[derive(Clone)]
pub(crate) struct WriterSender {
    /// One-slot command transport.
    commands: mpsc::SyncSender<WriterCommand>,
    /// Shared detached-output FIFO.
    detached: Arc<DetachedOutput>,
}

impl WriterSender {
    /// Sends one synchronous writer command with transport backpressure.
    pub(crate) fn send(&self, command: WriterCommand) -> ClientResult<()> {
        self.commands
            .send(command)
            .map_err(|_| ClientError::WriterClosed)
    }

    /// Admits one detached frame and wakes the writer when necessary.
    pub(crate) fn admit_detached(&self, frame: QueuedFrame) -> ClientResult<()> {
        self.detached.admit(frame)?;
        self.wake_detached()
    }

    /// Enables FIFO draining and wakes the writer.
    pub(crate) fn activate_detached(&self) -> ClientResult<()> {
        self.detached.activate()?;
        self.wake_detached()
    }

    /// Wakes the writer without coupling admission to command-lane capacity.
    fn wake_detached(&self) -> ClientResult<()> {
        match self.commands.try_send(WriterCommand::DetachedReady) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.detached.close();
                Err(ClientError::WriterClosed)
            }
        }
    }

    /// Closes further detached admission before shutdown.
    pub(crate) fn close_detached(&self) {
        self.detached.close();
    }
}

/// Receiving half of the writer transport and its shared detached FIFO.
pub(crate) struct WriterReceiver {
    /// One-slot command transport.
    commands: mpsc::Receiver<WriterCommand>,
    /// Shared detached-output FIFO.
    detached: Arc<DetachedOutput>,
}

/// Command sent from [`crate::ClientHandle`] clones to the writer thread.
pub(crate) enum WriterCommand {
    /// Write and flush a protocol frame, then acknowledge the result.
    Send(
        tau_proto::HarnessInputMessage,
        mpsc::Sender<ClientResult<()>>,
    ),
    /// Wake the writer to drain accepted detached frames.
    DetachedReady,
    /// Drain accepted detached frames before this acknowledged write.
    SendAfterDetached(
        tau_proto::HarnessInputMessage,
        mpsc::Sender<ClientResult<()>>,
    ),
    /// Flush any pending writer state and terminate the writer thread.
    Shutdown(mpsc::Sender<ClientResult<()>>),
}

/// Runs the blocking writer loop for one protocol peer.
pub(crate) fn run_writer<W>(writer: W, receiver: WriterReceiver) -> ClientResult<()>
where
    W: Write,
{
    let mut writer = tau_proto::PeerOutputWriter::new(writer);
    while let Ok(command) = receiver.commands.recv() {
        let shutdown = matches!(&command, WriterCommand::Shutdown(_));
        let result = match command {
            WriterCommand::Send(message, ack) => {
                let result = writer
                    .write_message(&message)
                    .map_err(ClientError::from)
                    .and_then(|()| writer.flush().map_err(ClientError::from));
                let should_stop = result.is_err();
                let ack_result = result.as_ref().copied().map_err(clone_error);
                let _ = ack.send(ack_result);
                if should_stop {
                    result
                } else {
                    drain_detached_batch(&mut writer, &receiver.detached)
                }
            }
            WriterCommand::DetachedReady => drain_detached_batch(&mut writer, &receiver.detached),
            WriterCommand::SendAfterDetached(message, ack) => {
                let result = drain_detached_batch(&mut writer, &receiver.detached).and_then(|()| {
                    writer
                        .write_message(&message)
                        .map_err(ClientError::from)
                        .and_then(|()| writer.flush().map_err(ClientError::from))
                });
                let ack_result = result.as_ref().copied().map_err(clone_error);
                let _ = ack.send(ack_result);
                result
            }
            WriterCommand::Shutdown(ack) => {
                receiver.detached.close();
                let result = drain_detached_all(&mut writer, &receiver.detached)
                    .and_then(|()| writer.flush().map_err(ClientError::from));
                let ack_result = result.as_ref().copied().map_err(clone_error);
                let _ = ack.send(ack_result);
                result
            }
        };
        if let Err(error) = result {
            receiver.detached.close();
            return Err(error);
        }
        if shutdown {
            return Ok(());
        }
    }
    receiver.detached.close();
    drain_detached_all(&mut writer, &receiver.detached)
}

/// Drains one captured FIFO batch so command handling cannot starve under
/// refill.
fn drain_detached_batch<W>(
    writer: &mut tau_proto::PeerOutputWriter<W>,
    detached: &DetachedOutput,
) -> ClientResult<()>
where
    W: Write,
{
    for _ in 0..detached.active_batch_len() {
        let Some(message) = detached.pop() else {
            break;
        };
        writer
            .write_message(&message)
            .map_err(ClientError::from)
            .and_then(|()| writer.flush().map_err(ClientError::from))?;
    }
    Ok(())
}

/// Drains every accepted frame after admission has closed.
fn drain_detached_all<W>(
    writer: &mut tau_proto::PeerOutputWriter<W>,
    detached: &DetachedOutput,
) -> ClientResult<()>
where
    W: Write,
{
    while detached.active_batch_len() != 0 {
        drain_detached_batch(writer, detached)?;
    }
    Ok(())
}

fn clone_error(error: &ClientError) -> ClientError {
    ClientError::handler(error.to_string())
}

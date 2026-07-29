use std::io::Write;
use std::sync::mpsc;

use crate::{ClientError, ClientResult};

/// Command sent from [`crate::ClientHandle`] clones to the writer thread.
pub(crate) enum WriterCommand {
    /// Write and flush a protocol frame, then acknowledge the result.
    Send(
        tau_proto::HarnessInputMessage,
        mpsc::Sender<ClientResult<()>>,
    ),
    /// Write and flush a protocol frame without reporting the result.
    SendDetached(tau_proto::HarnessInputMessage),
    /// Flush any pending writer state and terminate the writer thread.
    Shutdown(mpsc::Sender<ClientResult<()>>),
}

/// Runs the blocking writer loop for one protocol peer.
pub(crate) fn run_writer<W>(writer: W, receiver: mpsc::Receiver<WriterCommand>) -> ClientResult<()>
where
    W: Write,
{
    let mut writer = tau_proto::PeerOutputWriter::new(writer);
    while let Ok(command) = receiver.recv() {
        match command {
            WriterCommand::Send(message, ack) => {
                let result = writer
                    .write_message(&message)
                    .map_err(ClientError::from)
                    .and_then(|()| writer.flush().map_err(ClientError::from));
                let should_stop = result.is_err();
                let ack_result = result.as_ref().copied().map_err(clone_error);
                // This call is intentionally best-effort; preserve the existing discarded
                // result. ast-grep-ignore: let-underscore-call
                let _ = ack.send(ack_result);
                if should_stop {
                    return result;
                }
            }
            WriterCommand::SendDetached(message) => {
                writer
                    .write_message(&message)
                    .map_err(ClientError::from)
                    .and_then(|()| writer.flush().map_err(ClientError::from))?;
            }
            WriterCommand::Shutdown(ack) => {
                let result = writer.flush().map_err(ClientError::from);
                let ack_result = result.as_ref().copied().map_err(clone_error);
                // This call is intentionally best-effort; preserve the existing discarded
                // result. ast-grep-ignore: let-underscore-call
                let _ = ack.send(ack_result);
                return result;
            }
        }
    }
    Ok(())
}

fn clone_error(error: &ClientError) -> ClientError {
    ClientError::handler(error.to_string())
}

//! Background reader worker ownership and startup.

use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use tau_proto::{DecodeError, HarnessOutputMessage, PeerInputReader};

use crate::SocketTransportError;

/// Indivisible ownership of a socket peer's reader queue and background thread.
pub(super) struct ReaderWorker {
    /// Bounded queue of decoded harness-to-peer/client output messages.
    pub(super) frames: Receiver<Result<HarnessOutputMessage, DecodeError>>,
    /// Background reader thread that owns the read side of the socket.
    pub(super) thread: thread::JoinHandle<()>,
}

impl ReaderWorker {
    /// Starts a background reader with the protocol's one-frame queue capacity.
    pub(super) fn spawn(stream: UnixStream) -> Result<Self, SocketTransportError> {
        let (sender, frames) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("tau-socket-reader".to_owned())
            .spawn(move || read_frames(stream, sender))
            .map_err(|source| SocketTransportError::SpawnReader { source })?;
        Ok(Self { frames, thread })
    }

    /// Starts a reader that reports before blocking on a full queue in drop
    /// tests.
    #[cfg(test)]
    pub(super) fn spawn_with_blocked_enqueue_hook(
        stream: UnixStream,
        blocked_enqueue: SyncSender<()>,
    ) -> Result<Self, SocketTransportError> {
        let (sender, frames) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("tau-socket-reader".to_owned())
            .spawn(move || read_frames_until_blocked_enqueue(stream, sender, blocked_enqueue))
            .map_err(|source| SocketTransportError::SpawnReader { source })?;
        Ok(Self { frames, thread })
    }
}

/// Reads complete frames until the stream or receiving peer closes.
fn read_frames(stream: UnixStream, sender: SyncSender<Result<HarnessOutputMessage, DecodeError>>) {
    let mut reader = PeerInputReader::new(stream);
    loop {
        match reader.read_message() {
            Ok(Some(frame)) => {
                if sender.send(Ok(frame)).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        }
    }
}

/// Reads frames while exposing the full-queue point to the drop-order oracle.
#[cfg(test)]
fn read_frames_until_blocked_enqueue(
    stream: UnixStream,
    sender: SyncSender<Result<HarnessOutputMessage, DecodeError>>,
    blocked_enqueue: SyncSender<()>,
) {
    let mut reader = PeerInputReader::new(stream);
    loop {
        match reader.read_message() {
            Ok(Some(frame)) => match sender.try_send(Ok(frame)) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(frame)) => {
                    blocked_enqueue
                        .send(())
                        .expect("queue-drop test should wait for blocked enqueue");
                    if sender.send(frame).is_err() {
                        return;
                    }
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return,
            },
            Ok(None) => return,
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        }
    }
}

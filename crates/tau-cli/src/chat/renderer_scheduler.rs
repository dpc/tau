//! Fair arbitration between bounded remote work and local renderer commands.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::RendererCmd;

/// A local renderer command paired with the remote prefix that precedes it.
struct LocalRendererCmd {
    /// Presentation mutation requested by local input or timers.
    cmd: RendererCmd,
    /// Number of remote commands reserved when this command was submitted.
    remote_watermark: u64,
}

/// Scheduler-facing local receiver that keeps watermark envelopes private.
pub(super) struct LocalRendererReceiver {
    /// Underlying local envelope receiver.
    rx: mpsc::Receiver<LocalRendererCmd>,
}

impl LocalRendererReceiver {
    /// Receives one command without blocking for focused command tests.
    #[cfg(test)]
    pub(super) fn try_recv(&self) -> Result<RendererCmd, mpsc::TryRecvError> {
        self.rx.try_recv().map(|local| local.cmd)
    }
}

/// Captures a finite remote admission watermark for every local command.
#[derive(Clone)]
pub(super) struct LocalRendererSender {
    /// Unbounded local presentation-command channel.
    tx: mpsc::Sender<LocalRendererCmd>,
    /// Count of remote commands that have reserved FIFO admission.
    remote_admitted: Arc<AtomicU64>,
    /// Serializes local enqueue with scheduler channel arbitration.
    arbiter: Arc<Mutex<()>>,
}

impl LocalRendererSender {
    /// Creates the local sender and its scheduler-facing receiver.
    pub(super) fn channel(
        remote_admitted: Arc<AtomicU64>,
        arbiter: Arc<Mutex<()>>,
    ) -> (Self, LocalRendererReceiver) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                tx,
                remote_admitted,
                arbiter,
            },
            LocalRendererReceiver { rx },
        )
    }

    /// Submits a local command behind the remote prefix currently admitted.
    pub(super) fn send(&self, cmd: RendererCmd) -> Result<(), mpsc::SendError<RendererCmd>> {
        let _guard = self
            .arbiter
            .lock()
            .expect("renderer arbiter mutex poisoned");
        let local = LocalRendererCmd {
            cmd,
            remote_watermark: self.remote_admitted.load(Ordering::Acquire),
        };
        self.tx
            .send(local)
            .map_err(|error| mpsc::SendError(error.0.cmd))
    }
}

/// Owns renderer channels and fair finite-watermark arbitration state.
pub(super) struct RendererCommandScheduler {
    /// Bounded socket-to-renderer FIFO.
    remote_rx: mpsc::Receiver<RendererCmd>,
    /// Local commands carrying captured remote watermarks.
    local_rx: LocalRendererReceiver,
    /// Whether the remote producer has closed.
    remote_closed: bool,
    /// Number of remote commands returned to the renderer.
    remote_processed: u64,
    /// Local command waiting for its captured remote prefix.
    pending_local: Option<LocalRendererCmd>,
    /// Serializes local enqueue with nonblocking cross-channel selection.
    arbiter: Arc<Mutex<()>>,
}

impl RendererCommandScheduler {
    /// Creates a scheduler over production remote and local receivers.
    pub(super) fn new(
        remote_rx: mpsc::Receiver<RendererCmd>,
        local_rx: LocalRendererReceiver,
        arbiter: Arc<Mutex<()>>,
    ) -> Self {
        Self {
            remote_rx,
            local_rx,
            remote_closed: false,
            remote_processed: 0,
            pending_local: None,
            arbiter,
        }
    }

    /// Returns whether the remote producer has closed.
    pub(super) fn remote_closed(&self) -> bool {
        self.remote_closed
    }

    /// Returns the next command without overtaking or starving local work.
    pub(super) fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<RendererCmd, mpsc::RecvTimeoutError> {
        self.recv_timeout_inner(timeout, None)
    }

    /// Runs the scheduler with a one-shot test barrier after the local check.
    #[cfg(test)]
    pub(super) fn recv_timeout_after_local_check(
        &mut self,
        timeout: Duration,
        hook: &mut dyn FnMut(),
    ) -> Result<RendererCmd, mpsc::RecvTimeoutError> {
        self.recv_timeout_inner(timeout, Some(hook))
    }

    /// Implements iterative arbitration with an optional one-shot test barrier.
    fn recv_timeout_inner(
        &mut self,
        timeout: Duration,
        mut after_local_check: Option<&mut dyn FnMut()>,
    ) -> Result<RendererCmd, mpsc::RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        loop {
            let arbiter = self.arbiter.clone();
            let guard = arbiter.lock().expect("renderer arbiter mutex poisoned");
            if self.pending_local.is_none() {
                match self.local_rx.rx.try_recv() {
                    Ok(local) => self.pending_local = Some(local),
                    Err(mpsc::TryRecvError::Disconnected) if self.remote_closed => {
                        return Err(mpsc::RecvTimeoutError::Disconnected);
                    }
                    Err(mpsc::TryRecvError::Disconnected | mpsc::TryRecvError::Empty) => {}
                }
            }
            if self.pending_local.as_ref().is_some_and(|local| {
                self.remote_closed || local.remote_watermark <= self.remote_processed
            }) {
                return Ok(self
                    .pending_local
                    .take()
                    .expect("pending local command")
                    .cmd);
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if self.pending_local.is_some() {
                drop(guard);
                match self.remote_rx.recv_timeout(remaining) {
                    Ok(cmd) => return Ok(self.record_remote(cmd)),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        self.remote_closed = true;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            if self.remote_closed {
                drop(guard);
                return self
                    .local_rx
                    .rx
                    .recv_timeout(remaining)
                    .map(|local| local.cmd);
            }
            if let Some(hook) = after_local_check.take() {
                hook();
            }
            match self.remote_rx.try_recv() {
                Ok(cmd) => return Ok(self.record_remote(cmd)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.remote_closed = true;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    drop(guard);
                    match self.local_rx.rx.recv_timeout(remaining) {
                        Ok(local) => self.pending_local = Some(local),
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let cmd = self.remote_rx.recv_timeout(remaining)?;
                            return Ok(self.record_remote(cmd));
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            return match self.remote_rx.try_recv() {
                                Ok(cmd) => Ok(self.record_remote(cmd)),
                                Err(mpsc::TryRecvError::Disconnected) => {
                                    self.remote_closed = true;
                                    Err(mpsc::RecvTimeoutError::Timeout)
                                }
                                Err(mpsc::TryRecvError::Empty) => {
                                    Err(mpsc::RecvTimeoutError::Timeout)
                                }
                            };
                        }
                    }
                }
            }
        }
    }

    /// Records one remote command returned to the renderer.
    fn record_remote(&mut self, cmd: RendererCmd) -> RendererCmd {
        self.remote_processed = self.remote_processed.saturating_add(1);
        cmd
    }
}

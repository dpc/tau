//! Fair arbitration between bounded remote work and local renderer commands.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::RendererCmd;

/// Bounded socket-to-renderer sender that wakes the shared command scheduler.
pub(super) struct RemoteRendererSender {
    /// Underlying bounded remote FIFO, dropped before the final wake.
    tx: Option<mpsc::SyncSender<RendererCmd>>,
    /// Coalesced payload-free scheduler notification.
    wake: tau_blocking_notify_channel::Sender,
}

impl RemoteRendererSender {
    /// Creates the bounded remote FIFO and its scheduler-facing receiver.
    pub(super) fn channel(
        capacity: usize,
        wake: tau_blocking_notify_channel::Sender,
    ) -> (Self, mpsc::Receiver<RendererCmd>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (Self { tx: Some(tx), wake }, rx)
    }

    /// Enqueues one remote command and wakes the scheduler after success.
    pub(super) fn send(&self, cmd: RendererCmd) -> Result<(), mpsc::SendError<RendererCmd>> {
        let result = self.tx.as_ref().expect("live remote sender").send(cmd);
        if result.is_ok() {
            self.wake.notify();
        }
        result
    }
}

impl Drop for RemoteRendererSender {
    fn drop(&mut self) {
        drop(self.tx.take());
        self.wake.notify();
    }
}

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
pub(super) struct LocalRendererSender {
    /// Unbounded local presentation-command channel.
    tx: Option<mpsc::Sender<LocalRendererCmd>>,
    /// Count of remote commands that have reserved FIFO admission.
    remote_admitted: Arc<AtomicU64>,
    /// Serializes local enqueue with scheduler channel arbitration.
    arbiter: Arc<Mutex<()>>,
    /// Coalesced payload-free scheduler notification.
    wake: tau_blocking_notify_channel::Sender,
}

impl LocalRendererSender {
    /// Creates the local sender and its scheduler-facing receiver.
    pub(super) fn channel(
        remote_admitted: Arc<AtomicU64>,
        arbiter: Arc<Mutex<()>>,
        wake: tau_blocking_notify_channel::Sender,
    ) -> (Self, LocalRendererReceiver) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                tx: Some(tx),
                remote_admitted,
                arbiter,
                wake,
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
        let result = self
            .tx
            .as_ref()
            .expect("live local sender")
            .send(local)
            .map_err(|error| mpsc::SendError(error.0.cmd));
        if result.is_ok() {
            self.wake.notify();
        }
        result
    }
}

impl Clone for LocalRendererSender {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            remote_admitted: self.remote_admitted.clone(),
            arbiter: self.arbiter.clone(),
            wake: self.wake.clone(),
        }
    }
}

impl Drop for LocalRendererSender {
    fn drop(&mut self) {
        drop(self.tx.take());
        self.wake.notify();
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
    /// Shared coalesced wake receiver for both command sources.
    wake: tau_blocking_notify_channel::Receiver,
}

impl RendererCommandScheduler {
    /// Creates a scheduler over production remote and local receivers.
    pub(super) fn new(
        remote_rx: mpsc::Receiver<RendererCmd>,
        local_rx: LocalRendererReceiver,
        arbiter: Arc<Mutex<()>>,
        wake: tau_blocking_notify_channel::Receiver,
    ) -> Self {
        Self {
            remote_rx,
            local_rx,
            remote_closed: false,
            remote_processed: 0,
            pending_local: None,
            arbiter,
            wake,
        }
    }

    /// Returns whether the remote producer has closed.
    #[cfg(test)]
    pub(super) fn remote_closed(&self) -> bool {
        self.remote_closed
    }

    /// Returns the next command without overtaking or starving local work.
    pub(super) fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<RendererCmd, mpsc::RecvTimeoutError> {
        self.recv_timeout_inner(timeout, None, None, false)
    }

    /// Runs the scheduler with a one-shot test barrier after the local check.
    #[cfg(test)]
    pub(super) fn recv_timeout_after_local_check(
        &mut self,
        timeout: Duration,
        hook: &mut dyn FnMut(),
    ) -> Result<RendererCmd, mpsc::RecvTimeoutError> {
        self.recv_timeout_inner(timeout, Some(hook), None, false)
    }

    /// Runs one test barrier after both command sources were observed empty.
    #[cfg(test)]
    pub(super) fn recv_timeout_before_wait(
        &mut self,
        timeout: Duration,
        hook: &mut dyn FnMut(),
    ) -> Result<RendererCmd, mpsc::RecvTimeoutError> {
        self.recv_timeout_inner(timeout, None, Some(hook), false)
    }

    /// Repeats a test barrier before every attempted shared-wake wait.
    #[cfg(test)]
    pub(super) fn recv_timeout_before_each_wait(
        &mut self,
        timeout: Duration,
        hook: &mut dyn FnMut(),
    ) -> Result<RendererCmd, mpsc::RecvTimeoutError> {
        self.recv_timeout_inner(timeout, None, Some(hook), true)
    }

    /// Implements iterative arbitration with optional one-shot test barriers.
    fn recv_timeout_inner(
        &mut self,
        timeout: Duration,
        mut after_local_check: Option<&mut dyn FnMut()>,
        mut before_wait: Option<&mut dyn FnMut()>,
        repeat_before_wait: bool,
    ) -> Result<RendererCmd, mpsc::RecvTimeoutError> {
        let started_at = Instant::now();
        let mut deadline_elapsed = false;
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

            if self.pending_local.is_some() {
                match self.remote_rx.try_recv() {
                    Ok(cmd) => return Ok(self.record_remote(cmd)),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.remote_closed = true;
                        continue;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if self.remote_closed
                && self.pending_local.is_none()
                && matches!(
                    self.local_rx.rx.try_recv(),
                    Err(mpsc::TryRecvError::Disconnected)
                )
            {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            if let Some(hook) = after_local_check.take() {
                hook();
            }
            if !self.remote_closed {
                match self.remote_rx.try_recv() {
                    Ok(cmd) => return Ok(self.record_remote(cmd)),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.remote_closed = true;
                        continue;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            drop(guard);
            if repeat_before_wait {
                if let Some(hook) = before_wait.as_deref_mut() {
                    hook();
                }
            } else if let Some(hook) = before_wait.take() {
                hook();
            }
            if deadline_elapsed || started_at.elapsed() >= timeout {
                return Err(mpsc::RecvTimeoutError::Timeout);
            }
            let remaining = timeout.saturating_sub(started_at.elapsed());
            match self.wake.recv_timeout(remaining) {
                Ok(()) | Err(tau_blocking_notify_channel::RecvTimeoutError::Disconnected) => {}
                Err(tau_blocking_notify_channel::RecvTimeoutError::Timeout) => {
                    deadline_elapsed = true;
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

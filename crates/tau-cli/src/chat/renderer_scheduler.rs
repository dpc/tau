//! Fair arbitration between bounded remote work and local renderer commands.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use tau_proto::{Event, ProviderResponseStats};

use super::RendererCmd;
use super::cold_attach_stager::RendererPresentation;
use super::delivery_memory::{DeliveryMemoryCut, DeliveryMemoryTracker};

#[cfg(test)]
#[path = "renderer_scheduler/tests.rs"]
mod tests;

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
    /// Count of remote commands that have reserved FIFO admission.
    remote_admitted: Arc<AtomicU64>,
    /// Local commands carrying captured remote watermarks.
    local_rx: LocalRendererReceiver,
    /// Whether the remote producer has closed.
    remote_closed: bool,
    /// Number of remote commands returned to the renderer.
    remote_processed: u64,
    /// One non-foldable command read while inspecting an adjacent run.
    pending_remote: Option<RendererCmd>,
    /// Local command waiting for its captured remote prefix.
    pending_local: Option<LocalRendererCmd>,
    /// Serializes local enqueue with nonblocking cross-channel selection.
    arbiter: Arc<Mutex<()>>,
    /// Shared coalesced wake receiver for both command sources.
    wake: tau_blocking_notify_channel::Receiver,
    /// Enabled-only decoded-memory owner transitions.
    delivery_memory: Option<Arc<DeliveryMemoryTracker>>,
}

impl RendererCommandScheduler {
    /// Creates a scheduler over production remote and local receivers.
    pub(super) fn new(
        remote_rx: mpsc::Receiver<RendererCmd>,
        local_rx: LocalRendererReceiver,
        remote_admitted: Arc<AtomicU64>,
        arbiter: Arc<Mutex<()>>,
        wake: tau_blocking_notify_channel::Receiver,
        delivery_memory: Option<Arc<DeliveryMemoryTracker>>,
    ) -> Self {
        Self {
            remote_rx,
            remote_admitted,
            local_rx,
            remote_closed: false,
            remote_processed: 0,
            pending_remote: None,
            pending_local: None,
            arbiter,
            wake,
            delivery_memory,
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
                match self.try_recv_remote() {
                    Ok(cmd) => return Ok(self.dequeue_remote(cmd)),
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
                match self.try_recv_remote() {
                    Ok(cmd) => return Ok(self.dequeue_remote(cmd)),
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

    /// Returns a retained lookahead command before reading the bounded FIFO.
    fn try_recv_remote(&mut self) -> Result<RendererCmd, mpsc::TryRecvError> {
        let result = self
            .pending_remote
            .take()
            .map_or_else(|| self.remote_rx.try_recv(), Ok);
        if let Ok(
            RendererCmd::Remote { delivery_id, .. }
            | RendererCmd::RemoteDisconnect { delivery_id, .. },
        ) = &result
            && let Some(memory) = &self.delivery_memory
        {
            memory.transition(*delivery_id, DeliveryMemoryCut::Scheduler);
        }
        result
    }

    /// Records and opportunistically folds one already-admitted pure update
    /// run.
    fn dequeue_remote(&mut self, mut cmd: RendererCmd) -> RendererCmd {
        self.remote_processed = self.remote_processed.saturating_add(1);
        let captured = self.remote_admitted.load(Ordering::Acquire);
        let fold_before = self
            .pending_local
            .as_ref()
            .map_or(captured, |local| captured.min(local.remote_watermark));
        while self.remote_processed < fold_before && is_pure_provider_update(&cmd) {
            let next = match self.try_recv_remote() {
                Ok(next) => next,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.remote_closed = true;
                    break;
                }
            };
            match fold_provider_update(cmd, next) {
                (folded, None) => {
                    cmd = folded;
                    self.remote_processed = self.remote_processed.saturating_add(1);
                }
                (current, Some(barrier)) => {
                    cmd = current;
                    self.pending_remote = Some(barrier);
                    break;
                }
            }
        }
        cmd
    }
}

/// Returns whether a command carries an ordinary status-free provider update.
fn is_pure_provider_update(cmd: &RendererCmd) -> bool {
    matches!(
        cmd,
        RendererCmd::Remote {
            event,
            presentation: RendererPresentation::Ordinary,
            abandoned_shell_starts,
            ..
        } if abandoned_shell_starts.is_empty()
            && matches!(event.as_ref(), Event::ProviderResponseUpdated(update)
                if update.status.is_none() && update.compaction.is_none())
    )
}

/// Folds a matching adjacent update or returns both commands unchanged.
fn fold_provider_update(
    mut current: RendererCmd,
    next: RendererCmd,
) -> (RendererCmd, Option<RendererCmd>) {
    let RendererCmd::Remote {
        event: next_event,
        presentation: RendererPresentation::Ordinary,
        abandoned_shell_starts: next_abandoned,
        folded_frames: next_folded,
        ..
    } = &next
    else {
        return (current, Some(next));
    };
    if !next_abandoned.is_empty() || !next_folded.is_empty() {
        return (current, Some(next));
    }
    let Event::ProviderResponseUpdated(next_update) = next_event.as_ref() else {
        return (current, Some(next));
    };
    if next_update.status.is_some() || next_update.compaction.is_some() {
        return (current, Some(next));
    }
    let RendererCmd::Remote { event, .. } = &current else {
        return (current, Some(next));
    };
    let Event::ProviderResponseUpdated(update) = event.as_ref() else {
        return (current, Some(next));
    };
    if update.agent_id != next_update.agent_id
        || update.agent_prompt_id != next_update.agent_prompt_id
        || update.originator != next_update.originator
    {
        return (current, Some(next));
    }
    let RendererCmd::Remote {
        event: next_event,
        delivery_id,
        queue_bytes,
        enqueued_at,
        ..
    } = next
    else {
        unreachable!("validated remote update");
    };
    let Event::ProviderResponseUpdated(mut next_update) = *next_event else {
        unreachable!("validated provider response update");
    };
    let RendererCmd::Remote {
        event,
        folded_frames,
        ..
    } = &mut current
    else {
        unreachable!("validated current remote update");
    };
    let Event::ProviderResponseUpdated(update) = event.as_mut() else {
        unreachable!("validated current provider response update");
    };
    update.deltas.append(&mut next_update.deltas);
    fold_response_stats(&mut update.response_stats, next_update.response_stats);
    folded_frames.push(super::RendererQueueFrame {
        delivery_id,
        queue_bytes,
        enqueued_at,
    });
    (current, None)
}

/// Spans emitted samples while retaining the first observed semantic duration.
fn fold_response_stats(
    current: &mut Option<ProviderResponseStats>,
    next: Option<ProviderResponseStats>,
) {
    match (current.as_mut(), next) {
        (Some(current), Some(next)) => {
            current.current = next.current;
            current.first_semantic_output_elapsed_micros = current
                .first_semantic_output_elapsed_micros
                .or(next.first_semantic_output_elapsed_micros);
        }
        (None, Some(next)) => *current = Some(next),
        (_, None) => {}
    }
}

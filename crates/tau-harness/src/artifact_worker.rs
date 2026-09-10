//! Bounded artifact I/O admission and completion delivery.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tau_proto::{ArtifactError, ArtifactRequest, ArtifactResult, ConnectionId};

use crate::artifact_store::ArtifactStore;
use crate::event::{HarnessCommand, HarnessEvent};

#[cfg(test)]
mod tests;

/// Includes queued work, in-flight filesystem work, and unconsumed completions.
const REQUEST_LIMIT: usize = 8;

/// Lazy harness-lifetime worker handle; all filesystem work stays off-loop.
pub(crate) struct ArtifactWorker {
    /// Bounded FIFO into the sole store owner.
    tx: mpsc::SyncSender<Job>,
    /// Permit counter spanning both request and completion queues.
    pending: Arc<AtomicUsize>,
    /// Revocable connection generations, not public digest authority.
    connections: HashMap<ConnectionId, Arc<AtomicBool>>,
}

/// Request and its captured admission authority.
struct Job {
    /// Configured instance plus exact session, derived by the harness.
    owner: String,
    /// Connection generation that submitted the request.
    connection: ConnectionId,
    /// Revoked at disconnect even while filesystem work is running.
    alive: Arc<AtomicBool>,
    /// Typed bounded request; bytes remain private.
    request: ArtifactRequest,
    /// Keeps completion delivery within the same finite admission bound.
    permit: Permit,
}

/// Admission remains charged until the central loop consumes its completion.
pub(crate) struct Permit(
    /// Shared count decremented when this request or completion leaves custody.
    Arc<AtomicUsize>,
);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Private non-event completion delivered through the existing runtime channel.
pub(crate) struct ArtifactCompleted {
    /// Exact connection generation to receive the response.
    pub(crate) connection: ConnectionId,
    /// Transfer response, never a canonical event.
    pub(crate) result: ArtifactResult,
    /// Request budget retained until completion routing finishes.
    pub(crate) _permit: Permit,
}

impl ArtifactWorker {
    /// Starts one worker without opening or inspecting the persistent root.
    pub(crate) fn start(
        root: &Path,
        completion: mpsc::Sender<HarnessEvent>,
    ) -> Result<Self, ArtifactError> {
        let mut store = ArtifactStore::new(root);
        let (tx, rx) = mpsc::sync_channel::<Job>(REQUEST_LIMIT);
        thread::Builder::new()
            .name("tau-artifacts".to_owned())
            .spawn(move || {
                let mut connections = HashMap::<ConnectionId, Arc<AtomicBool>>::new();
                loop {
                    connections.retain(|connection, alive| {
                        if alive.load(Ordering::Acquire) {
                            true
                        } else {
                            store.disconnect(connection.as_str());
                            false
                        }
                    });
                    store.expire(Instant::now());
                    let job = match rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    if !job.alive.load(Ordering::Acquire) {
                        store.disconnect(job.connection.as_str());
                        continue;
                    }
                    connections.insert(job.connection.clone(), Arc::clone(&job.alive));
                    let result = store.execute(
                        &job.owner,
                        job.connection.as_str(),
                        job.request.op,
                        unix_now(),
                    );
                    // Cancellation/disconnect may race a committed publication.
                    // Never delete its object or undo a
                    // shared duplicate's age update.
                    let result = ArtifactResult {
                        request_id: job.request.request_id,
                        result,
                    };
                    let command = ArtifactCompleted {
                        connection: job.connection,
                        result,
                        _permit: job.permit,
                    };
                    if completion
                        .send(HarnessEvent::Command(HarnessCommand::ArtifactCompleted(
                            Box::new(command),
                        )))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|_| ArtifactError::Io)?;
        Ok(Self {
            tx,
            pending: Arc::new(AtomicUsize::new(0)),
            connections: HashMap::new(),
        })
    }

    /// Rejects overload immediately; accepted bytes and completion stay
    /// bounded.
    pub(crate) fn submit(
        &mut self,
        owner: String,
        connection: ConnectionId,
        request: ArtifactRequest,
    ) -> Result<(), ArtifactError> {
        self.pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < REQUEST_LIMIT).then_some(count + 1)
            })
            .map_err(|_| ArtifactError::Busy)?;
        let permit = Permit(Arc::clone(&self.pending));
        let alive = Arc::clone(
            self.connections
                .entry(connection.clone())
                .or_insert_with(|| Arc::new(AtomicBool::new(true))),
        );
        self.tx
            .try_send(Job {
                owner,
                connection,
                alive,
                request,
                permit,
            })
            .map_err(|_| ArtifactError::Busy)
    }

    /// Revokes unfinished transfer authority immediately without waiting for
    /// I/O.
    pub(crate) fn disconnect(&mut self, connection: &ConnectionId) {
        if let Some(alive) = self.connections.remove(connection) {
            alive.store(false, Ordering::Release);
        }
    }
}

impl Drop for ArtifactWorker {
    fn drop(&mut self) {
        for alive in self.connections.values() {
            alive.store(false, Ordering::Release);
        }
    }
}

/// Wall-clock seconds used solely for artifact age and retry receipt expiry.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

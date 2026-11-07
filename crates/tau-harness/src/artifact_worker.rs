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
use crate::image_verification::ImageVerificationCut;
use crate::image_verification::{ImageVerificationCompleted, ImageVerificationRequest};

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

/// Both work families share the same queue and outstanding-operation budget.
enum Job {
    /// Existing directed extension/UI transfer work.
    Artifact(Box<ArtifactJob>),
    /// Internal prepared-image verification, never an extension pin operation.
    Image(Box<ImageJob>),
}

/// Directed request and its captured admission authority.
struct ArtifactJob {
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

/// One internal verification job whose physical guard never leaves the worker.
struct ImageJob {
    /// Exact tool result and its captured runtime/lease ownership.
    request: ImageVerificationRequest,
    /// Revoked independently when this configured connection disconnects.
    alive: Arc<AtomicBool>,
    /// Shared permit retained through completion consumption.
    permit: Permit,
}

#[cfg(test)]
type ImageHook = Arc<dyn Fn(ImageVerificationCut) + Send + Sync>;

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
    #[cfg(test)]
    pub(crate) fn start(
        root: &Path,
        completion: mpsc::Sender<HarnessEvent>,
    ) -> Result<Self, ArtifactError> {
        Self::start_inner(
            root,
            completion,
            tau_proto::ArtifactLimits::default().stored_bytes,
            #[cfg(test)]
            None,
        )
    }

    /// Installs deterministic physical-I/O cuts without affecting production.
    #[cfg(test)]
    pub(crate) fn start_with_image_hook(
        root: &Path,
        completion: mpsc::Sender<HarnessEvent>,
        hook: ImageHook,
    ) -> Result<Self, ArtifactError> {
        Self::start_inner(
            root,
            completion,
            tau_proto::ArtifactLimits::default().stored_bytes,
            Some(hook),
        )
    }

    /// Starts a worker with the accepted harness original-byte policy.
    pub(crate) fn start_with_limit(
        root: &Path,
        completion: mpsc::Sender<HarnessEvent>,
        max_bytes: tau_proto::ArtifactSize,
    ) -> Result<Self, ArtifactError> {
        Self::start_inner(
            root,
            completion,
            max_bytes,
            #[cfg(test)]
            None,
        )
    }

    fn start_inner(
        root: &Path,
        completion: mpsc::Sender<HarnessEvent>,
        max_bytes: tau_proto::ArtifactSize,
        #[cfg(test)] image_hook: Option<ImageHook>,
    ) -> Result<Self, ArtifactError> {
        let mut store = ArtifactStore::new(root);
        store.set_max_bytes(max_bytes);
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
                    let command = match job {
                        Job::Artifact(job) => {
                            if !job.alive.load(Ordering::Acquire) {
                                store.disconnect(job.connection.as_str());
                                continue;
                            }
                            connections.insert(job.connection.clone(), Arc::clone(&job.alive));
                            let result = match job.request.op {
                                tau_proto::ArtifactOp::List { prefix, limit } => {
                                    store.list(&prefix, limit, || job.alive.load(Ordering::Acquire))
                                }
                                op => store.execute(
                                    &job.owner,
                                    job.connection.as_str(),
                                    op,
                                    unix_now(),
                                ),
                            };
                            HarnessCommand::ArtifactCompleted(Box::new(ArtifactCompleted {
                                connection: job.connection,
                                result: ArtifactResult {
                                    request_id: job.request.request_id,
                                    result,
                                },
                                _permit: job.permit,
                            }))
                        }
                        Job::Image(job) => {
                            connections.insert(
                                job.request.owner.connection.clone(),
                                Arc::clone(&job.alive),
                            );
                            // A decoder panic must unwind scoped lock custody
                            // and fail this call, not lose the
                            // worker/completion.
                            let outcome =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    job.request.verify(&store, &job.alive, |cut| {
                                        #[cfg(test)]
                                        if let Some(hook) = &image_hook {
                                            hook(cut);
                                        }
                                        #[cfg(not(test))]
                                        let _ = cut;
                                    })
                                }))
                                .unwrap_or(Err(ArtifactError::Io));
                            HarnessCommand::ImageVerificationCompleted(Box::new(
                                ImageVerificationCompleted {
                                    request: job.request,
                                    outcome,
                                    _permit: job.permit,
                                },
                            ))
                        }
                    };
                    // Cancellation/disconnect may race a committed publication.
                    // Never roll back a shared object or conservative root.
                    if completion.send(HarnessEvent::Command(command)).is_err() {
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
        let permit = self.reserve()?;
        let alive = self.connection_live(&connection);
        self.tx
            .try_send(Job::Artifact(Box::new(ArtifactJob {
                owner,
                connection,
                alive,
                request,
                permit,
            })))
            .map_err(|_| ArtifactError::Busy)
    }

    /// Admits internal verification under the same eight-operation limit as
    /// RPC.
    pub(crate) fn submit_image(
        &mut self,
        request: ImageVerificationRequest,
    ) -> Result<(), ArtifactError> {
        request.validate_shape()?;
        let permit = self.reserve()?;
        let alive = self.connection_live(&request.owner.connection);
        self.tx
            .try_send(Job::Image(Box::new(ImageJob {
                request,
                alive,
                permit,
            })))
            .map_err(|_| ArtifactError::Busy)
    }

    /// Charges queued, active and unconsumed work uniformly.
    fn reserve(&self) -> Result<Permit, ArtifactError> {
        self.pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < REQUEST_LIMIT).then_some(count + 1)
            })
            .map_err(|_| ArtifactError::Busy)?;
        Ok(Permit(Arc::clone(&self.pending)))
    }

    /// Shares one revocation token with every operation on this connection.
    fn connection_live(&mut self, connection: &ConnectionId) -> Arc<AtomicBool> {
        Arc::clone(
            self.connections
                .entry(connection.clone())
                .or_insert_with(|| Arc::new(AtomicBool::new(true))),
        )
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

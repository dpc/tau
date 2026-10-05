//! Finite, generation-bound journal reads outside the runtime loop.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::{io, thread};

use tau_core::{AgentHistoryPrefix, AgentStoreError, PrefetchedAgentHistory};

use crate::event::{HarnessCommand, HarnessEvent};

/// Queued, active and unconsumed reads share the same finite admission budget.
const REQUEST_LIMIT: usize = 8;

/// One lazy reader; admission and cancellation never wait for filesystem I/O.
pub(crate) struct HistoryReader {
    /// Finite FIFO of metadata-only prefix capabilities.
    tx: mpsc::SyncSender<Job>,
    /// Includes physical work whose logical request was canceled.
    outstanding: Arc<AtomicUsize>,
}

/// A batch uses one permit until the runtime consumes its completion.
struct Job {
    /// Runtime-owned continuation identity.
    id: u64,
    /// Exact finite prefix capabilities, not open-file owners.
    prefixes: Vec<AgentHistoryPrefix>,
    /// Logical cancellation cannot interrupt an already executing filesystem
    /// read.
    canceled: Arc<AtomicBool>,
    /// Follows the physical job through completion.
    permit: Permit,
}

/// Releases admission only when physical work and completion custody have
/// ended.
pub(crate) struct Permit(
    /// Shared total including queued and unconsumed work.
    Arc<AtomicUsize>,
);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Read result sent through the existing nonsemantic runtime command channel.
pub(crate) struct HistoryReadCompleted {
    /// Exact continuation to resume, if it still exists.
    pub(crate) id: u64,
    /// `None` means cancellation observed before or between physical reads.
    pub(crate) result: Result<Option<Vec<PrefetchedAgentHistory>>, HistoryReadFailure>,
    /// Keeps result backlog within the same limit as reader admission.
    pub(crate) _permit: Permit,
}

/// Keeps generation authority with a failed read for runtime-side revalidation.
pub(crate) struct HistoryReadFailure {
    /// The request may have lost authority after its worker-side postcheck.
    pub(crate) prefix: AgentHistoryPrefix,
    /// Actual read/validation error; not an empty successful history.
    pub(crate) error: AgentStoreError,
}

impl HistoryReader {
    /// Starts the sole worker without opening any journal on the caller thread.
    pub(crate) fn start(completion: mpsc::Sender<HarnessEvent>) -> io::Result<Self> {
        Self::start_with_reader(completion, AgentHistoryPrefix::prefetch)
    }

    /// Shared production/test constructor; injected readers provide
    /// deterministic physical-work cuts without timing-based tests.
    pub(crate) fn start_with_reader(
        completion: mpsc::Sender<HarnessEvent>,
        read: impl Fn(&AgentHistoryPrefix) -> Result<PrefetchedAgentHistory, AgentStoreError>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Job>(REQUEST_LIMIT);
        thread::Builder::new()
            .name("tau-history-reader".to_owned())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    let result = read_batch(&job, &read);
                    let command = HistoryReadCompleted {
                        id: job.id,
                        result,
                        _permit: job.permit,
                    };
                    if completion
                        .send(HarnessEvent::Command(HarnessCommand::HistoryReadCompleted(
                            Box::new(command),
                        )))
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            tx,
            outstanding: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Rejects overload immediately. The returned token cancels logical work
    /// but does not release the physical job's permit.
    pub(crate) fn submit(
        &self,
        id: u64,
        prefixes: Vec<AgentHistoryPrefix>,
    ) -> Result<Arc<AtomicBool>, ()> {
        self.outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < REQUEST_LIMIT).then_some(count + 1)
            })
            .map_err(|_| ())?;
        let canceled = Arc::new(AtomicBool::new(false));
        self.tx
            .try_send(Job {
                id,
                prefixes,
                canceled: Arc::clone(&canceled),
                permit: Permit(Arc::clone(&self.outstanding)),
            })
            .map_err(|_| ())?;
        Ok(canceled)
    }
}

/// Cancellation drops materialized prefixes on the reader, while real errors
/// keep their exact authority for classification by the runtime.
fn read_batch(
    job: &Job,
    read: &impl Fn(&AgentHistoryPrefix) -> Result<PrefetchedAgentHistory, AgentStoreError>,
) -> Result<Option<Vec<PrefetchedAgentHistory>>, HistoryReadFailure> {
    let mut histories = Vec::with_capacity(job.prefixes.len());
    for prefix in &job.prefixes {
        if job.canceled.load(Ordering::Acquire) {
            return Ok(None);
        }
        histories.push(read(prefix).map_err(|error| HistoryReadFailure {
            prefix: prefix.clone(),
            error,
        })?);
    }
    if job.canceled.load(Ordering::Acquire) {
        return Ok(None);
    }
    Ok(Some(histories))
}

#[cfg(test)]
mod tests;

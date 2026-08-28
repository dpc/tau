//! Persistence owner, typed registry, and failure-atomic admission.

#![expect(
    dead_code,
    reason = "store migration in this change consumes the complete staged API"
)]

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak, mpsc};
use std::time::Duration;
use std::{fmt, mem, thread};

use super::backend::{FilesystemBackend, PersistenceBackend};
use super::capacity::PersistenceCapacity;
use super::identity::{LeaseIdentity, PersistenceGeneration, PersistenceLease, StreamIdentity};
use super::preparation::{
    PreparationResult, PreparedAgentStream, PreparedSessionStreams, SessionPreparationMode,
    WorkerCommand,
};
use super::worker::{
    FrameJob, StagedFrameKind, StreamLifecycle, worker_internal_charge, worker_main,
};

static NEXT_OWNER_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Positive owner-local FIFO identity assigned to one admitted frame.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct FrameAdmissionToken(u64);

impl FrameAdmissionToken {
    /// First identity allocated by a newly created persistence owner.
    const FIRST: Self = Self(1);

    /// Advances the allocation cursor while preserving the existing exhaustion
    /// policy.
    fn checked_successor(self) -> Self {
        Self(self.0.checked_add(1).expect("global frame token exhausted"))
    }
}

/// Admission failure returned before canonical acceptance changes any live
/// fact.
#[derive(Debug)]
pub enum PersistenceAdmissionError {
    /// The exact aggregate frame or byte boundary is full.
    Full,
    /// The worker exited or the owner is shutting down.
    Unavailable,
    /// The lease no longer names the prepared generation.
    StaleLease,
    /// The stream has not completed explicit preparation.
    NotPrepared,
    /// Explicit existing-stream preparation found no canonical journal.
    StreamNotFound,
    /// The stream is poisoned after rollback could not be proven.
    Poisoned,
    /// The stream failed non-destructive first creation.
    CreationFailed,
    /// A synchronous lifecycle operation failed.
    Lifecycle(String),
}

/// Typed asynchronous persistence failure classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceFailureKind {
    /// Stream path or file creation/open failed.
    Open,
    /// Exclusive stream lock acquisition failed.
    Lock,
    /// Authoritative frame write failed but rollback was proven.
    Write,
    /// Exact EOF rollback could not be proven.
    Rollback,
    /// Non-destructive new-agent creation found a pre-existing canonical path.
    Collision,
    /// A prior terminal failure discarded a later same-generation frame.
    GenerationFailed,
    /// Journal or checkpoint durability synchronization failed.
    Sync,
    /// The unique worker exited and invalidated every generation.
    WorkerExit,
}

/// Capacity boundary which rejected one nonblocking semantic publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceCapacityLimit {
    /// The retained authoritative-frame count reached its configured limit.
    Frames,
    /// The aggregate retained-byte ledger lacked the requested bytes.
    Bytes,
    /// The prepared-stream count reached its configured limit.
    Streams,
    /// A deterministic test injected the same pre-publication rejection.
    Injected,
}

/// Content-free exact persistence resource totals at an operational transition.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PersistenceUsage {
    /// Staged, queued, and in-flight authoritative frames.
    pub frames: usize,
    /// Aggregate retained bytes, including registry and durability debt.
    pub bytes: usize,
    /// Registered stream generations.
    pub streams: usize,
}

/// One edge-triggered capacity-pressure observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistenceCapacityPressure {
    /// Capacity boundary which rejected publication.
    pub limit: PersistenceCapacityLimit,
    /// Exact resource totals at rejection.
    pub usage: PersistenceUsage,
}

/// Drained content-free operational state from the persistence owner.
#[derive(Default)]
pub struct PersistenceOperationalStatus {
    /// Exact resource totals at the drain cut.
    pub usage: PersistenceUsage,
    /// Bounded asynchronous worker failures since the previous drain.
    pub failures: Vec<PersistenceFailure>,
    /// First capacity-full transition since the previous recovery.
    pub capacity_full: Option<PersistenceCapacityPressure>,
    /// Exact totals after capacity became available again.
    pub recovered: Option<PersistenceUsage>,
    /// Exact totals after the previously pressured frame FIFO fully drained.
    pub drained: Option<PersistenceUsage>,
}

/// Bounded content-free asynchronous persistence failure.
#[derive(Clone)]
pub struct PersistenceFailure {
    /// Exact stream capability, absent only for whole-owner worker exit.
    identity: Option<Arc<LeaseIdentity>>,
    /// Stable failure classification.
    kind: PersistenceFailureKind,
}

impl PersistenceFailure {
    /// Returns the affected stream, when the failure is stream-local.
    #[must_use]
    pub fn stream(&self) -> Option<&StreamIdentity> {
        self.identity.as_ref().map(|identity| &identity.stream)
    }

    /// Returns the affected generation, when the failure is stream-local.
    #[must_use]
    pub fn generation(&self) -> Option<PersistenceGeneration> {
        self.identity.as_ref().map(|identity| identity.generation)
    }

    /// Returns the typed failure classification.
    #[must_use]
    pub const fn kind(&self) -> PersistenceFailureKind {
        self.kind
    }
}

impl fmt::Display for PersistenceAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("semantic persistence admission is full"),
            Self::Unavailable => formatter.write_str("semantic persistence is unavailable"),
            Self::StaleLease => formatter.write_str("semantic persistence lease is stale"),
            Self::NotPrepared => formatter.write_str("semantic persistence stream is not prepared"),
            Self::StreamNotFound => {
                formatter.write_str("semantic persistence stream was not found")
            }
            Self::Poisoned => formatter.write_str("semantic persistence stream is poisoned"),
            Self::CreationFailed => {
                formatter.write_str("semantic persistence stream creation failed")
            }
            Self::Lifecycle(message) => {
                write!(
                    formatter,
                    "semantic persistence lifecycle operation failed: {message}"
                )
            }
        }
    }
}

impl std::error::Error for PersistenceAdmissionError {}

/// Conservative typed accounting for everything one append can retain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RetentionCharge {
    /// Encoded frame vector capacity plus its length prefix.
    pub(crate) frame: usize,
    /// Complete off-side fold/cache/projection replacement.
    pub(crate) replacement: usize,
    /// Optional bounded checkpoint candidate capacity.
    pub(crate) checkpoint: usize,
    /// Caller-owned auxiliary projection vectors retained by the replacement.
    pub(crate) projections: usize,
}

impl RetentionCharge {
    /// Computes the checked aggregate reservation.
    fn total(self) -> Option<usize> {
        [
            self.frame,
            self.replacement,
            self.checkpoint,
            self.projections,
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
    }
}

/// Fully encoded frame built only after count and byte reservations succeed.
pub(crate) struct StagedFrame {
    /// Encoded CBOR payload.
    pub(crate) payload: Vec<u8>,
    /// Optional complete checkpoint replacement.
    pub(crate) checkpoint: Option<super::worker::AgentCheckpointCandidate>,
    /// Typed semantic role of this frame.
    pub(crate) kind: StagedFrameKind,
}

/// Count permit acquired before any serialized-size counting pass.
pub(crate) struct FrameReservation {
    /// Exact owner state and ledger holding this reservation.
    shared: Arc<Shared>,
    /// Allocation-free shared exact capability identity.
    identity: Arc<LeaseIdentity>,
    /// Whether this permit transferred into a byte reservation.
    transferred: bool,
}

impl FrameReservation {
    /// Reserves aggregate retained bytes before encoding, cloning, or folding.
    pub(crate) fn reserve_bytes(
        mut self,
        charge: RetentionCharge,
    ) -> Result<StagingReservation, PersistenceAdmissionError> {
        let caller_bytes = charge.total().ok_or(PersistenceAdmissionError::Full)?;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        validate_generation(
            &state,
            self.shared.owner_epoch,
            self.identity.owner_epoch,
            &self.identity.stream,
            self.identity.generation,
        )?;
        let registered = state
            .streams
            .get(&self.identity.stream)
            .expect("validated stream exists");
        let internal_bytes = internal_job_charge(registered);
        let bytes = caller_bytes
            .checked_add(internal_bytes)
            .ok_or(PersistenceAdmissionError::Full)?;
        if bytes
            > self
                .shared
                .capacity
                .max_bytes
                .saturating_sub(state.ledger.bytes)
        {
            state.ledger.frames = state
                .ledger
                .frames
                .checked_sub(1)
                .expect("byte rejection releases its frame reservation");
            self.transferred = true;
            report_capacity_full(&self.shared, &mut state, PersistenceCapacityLimit::Bytes);
            return Err(PersistenceAdmissionError::Full);
        }
        state.ledger.bytes += bytes;
        self.transferred = true;
        drop(state);
        Ok(StagingReservation {
            shared: Arc::clone(&self.shared),
            identity: Arc::clone(&self.identity),
            charge,
            bytes,
            internal_bytes,
            committed: false,
        })
    }
}

impl Drop for FrameReservation {
    fn drop(&mut self) {
        if self.transferred {
            return;
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ledger.frames = state
            .ledger
            .frames
            .checked_sub(1)
            .expect("frame reservation released exactly once");
        report_capacity_recovered(&self.shared, &mut state);
        self.shared.wake.notify_all();
    }
}

/// Aggregate byte permit held while fallible staging occurs off-side.
pub(crate) struct StagingReservation {
    /// Exact owner state and ledger holding this reservation.
    shared: Arc<Shared>,
    /// Allocation-free shared exact capability identity.
    identity: Arc<LeaseIdentity>,
    /// Typed charge checked against actual prebuilt capacities at commit.
    charge: RetentionCharge,
    /// Exact aggregate bytes reserved.
    bytes: usize,
    /// Foundation-owned fixed/key/handle/debt charge.
    internal_bytes: usize,
    /// Whether permits transferred into the committed FIFO.
    committed: bool,
}

impl StagingReservation {
    /// Atomically swaps one complete replacement and inserts one prebuilt FIFO
    /// node.
    ///
    /// The commit contains only generation checks, `mem::swap`, and insertion
    /// into storage preallocated to the hard frame limit.
    pub(crate) fn commit_swap<T>(
        mut self,
        target: &mut T,
        mut replacement: T,
        frame: StagedFrame,
    ) -> Result<(), PersistenceAdmissionError> {
        frame.validate_charge(self.charge)?;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        validate_generation(
            &state,
            self.shared.owner_epoch,
            self.identity.owner_epoch,
            &self.identity.stream,
            self.identity.generation,
        )?;
        let lifecycle = state
            .streams
            .get(&self.identity.stream)
            .expect("validated stream remains registered")
            .lifecycle;
        match (lifecycle, &frame.kind) {
            (StreamLifecycle::ReservedNewAgent, StagedFrameKind::FirstAgent(_)) => {
                state
                    .streams
                    .get_mut(&self.identity.stream)
                    .expect("validated stream remains registered")
                    .lifecycle = StreamLifecycle::CreationQueued;
            }
            (StreamLifecycle::CreationQueued, StagedFrameKind::Ordinary)
            | (StreamLifecycle::Prepared, StagedFrameKind::Ordinary) => {}
            (StreamLifecycle::ReservedNewAgent, StagedFrameKind::Ordinary) => {
                return Err(PersistenceAdmissionError::NotPrepared);
            }
            (
                StreamLifecycle::CreationQueued | StreamLifecycle::Prepared,
                StagedFrameKind::FirstAgent(_),
            ) => return Err(PersistenceAdmissionError::StaleLease),
            (lifecycle, _) => return Err(lifecycle_admission_error(lifecycle)),
        }
        let admission_watermark = state.next_frame_token;
        state.next_frame_token = admission_watermark.checked_successor();
        state.last_admitted_frame = Some(admission_watermark);
        let mut job = FrameJob::from_reservation(
            Arc::clone(&self.identity),
            frame,
            self.charge,
            self.bytes,
            self.internal_bytes,
            admission_watermark,
        );
        /*
         * The lifecycle match above is deliberately exhaustive. Keep the swap and
         * accounting transfer and preallocated insertion adjacent: no fallible
         * or destructor work belongs after this cut.
         */
        mem::swap(target, &mut replacement);
        job.release_staging_replacement();
        state.frames.push_back(job);
        self.committed = true;
        drop(state);
        self.shared.wake.notify_one();
        drop(replacement);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ledger.bytes = state
            .ledger
            .bytes
            .checked_sub(self.charge.replacement)
            .expect("committed swap releases retired replacement staging");
        report_capacity_recovered(&self.shared, &mut state);
        Ok(())
    }
}

impl Drop for StagingReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ledger.frames = state
            .ledger
            .frames
            .checked_sub(1)
            .expect("staging frame released exactly once");
        state.ledger.bytes = state
            .ledger
            .bytes
            .checked_sub(self.bytes)
            .expect("staging bytes released exactly once");
        report_capacity_recovered(&self.shared, &mut state);
        self.shared.wake.notify_all();
    }
}

/// One Harness-lifecycle owner of all durable semantic streams.
pub struct SemanticPersistenceOwner {
    /// Shared registry, ledger, FIFO, and wakeup.
    shared: Arc<Shared>,
    /// Worker handle retained only to keep one explicit lifecycle owner.
    worker: Option<thread::JoinHandle<()>>,
}

impl fmt::Debug for SemanticPersistenceOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SemanticPersistenceOwner")
            .field("owner_epoch", &self.shared.owner_epoch)
            .finish_non_exhaustive()
    }
}

pub(crate) struct Shared {
    /// Unique process-local owner epoch.
    pub(crate) owner_epoch: u64,
    /// Hard aggregate capacity.
    pub(crate) capacity: PersistenceCapacity,
    /// Injectable production filesystem boundary.
    pub(crate) backend: Arc<dyn PersistenceBackend>,
    /// Registry, exact ledger, and committed FIFO.
    pub(crate) state: Mutex<AdmissionState>,
    /// Notification paired with state and retry deadlines.
    pub(crate) wake: Condvar,
    /// Content-free harness-loop wake installed by the lifecycle owner.
    operational_wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Coalesces arbitrarily many bounded outcomes into one outstanding wake.
    operational_wake_pending: AtomicBool,
}

pub(crate) struct AdmissionState {
    /// False after shutdown begins or every worker exit.
    pub(crate) available: bool,
    /// Whether owner-initiated shutdown explains the worker exit.
    pub(crate) shutting_down: bool,
    /// Set after the sole worker dropped every mutable filesystem handle.
    pub(crate) worker_exited: bool,
    /// One-shot deterministic test rejection before count admission.
    pub(crate) rejected_admissions_remaining: usize,
    /// Successful admissions to allow before the injected rejection run.
    pub(crate) admissions_before_rejection: usize,
    /// Next generation allocated within this owner.
    pub(crate) next_generation: u64,
    /// Next owner-local authoritative FIFO acceptance token.
    pub(crate) next_frame_token: FrameAdmissionToken,
    /// Most recently allocated authoritative FIFO acceptance token.
    pub(crate) last_admitted_frame: Option<FrameAdmissionToken>,
    /// Most recent FIFO disposition, sufficient because touch captures latest
    /// admission.
    pub(crate) last_frame_disposition: Option<(FrameAdmissionToken, bool)>,
    /// Exact state of every registered stream.
    pub(crate) streams: HashMap<StreamIdentity, RegisteredStream>,
    /// Preallocated authoritative FIFO.
    pub(crate) frames: VecDeque<FrameJob>,
    /// Preallocated typed lifecycle command queue.
    pub(crate) commands: VecDeque<WorkerCommand>,
    /// Exact aggregate retained-resource ledger.
    pub(crate) ledger: ResourceLedger,
    /// Bounded content-free worker outcomes for diagnostics and deterministic
    /// tests.
    pub(crate) failures: VecDeque<PersistenceFailure>,
    /// Active edge-triggered capacity pressure.
    capacity_pressure: Option<PersistenceCapacityPressure>,
    /// Newly entered pressure edge awaiting observation.
    capacity_full_pending: Option<PersistenceCapacityPressure>,
    /// Most recent recovery edge awaiting observation.
    capacity_recovered: Option<PersistenceUsage>,
    /// Whether a recovery cycle still owns an eventual drained edge.
    recovery_in_progress: bool,
    /// Most recent fully drained recovery edge awaiting observation.
    capacity_drained: Option<PersistenceUsage>,
}

#[derive(Default)]
pub(crate) struct ResourceLedger {
    /// Staged, queued, and in-flight authoritative frames.
    pub(crate) frames: usize,
    /// Aggregate staging, queue, registry, handle, and debt bytes.
    pub(crate) bytes: usize,
    /// Registered stream/handle permits.
    pub(crate) streams: usize,
}

pub(crate) struct RegisteredStream {
    /// Current generation for stale-writer rejection.
    pub(crate) generation: PersistenceGeneration,
    /// Explicit lifecycle state.
    pub(crate) lifecycle: StreamLifecycle,
    /// Journal path retained and charged for this generation.
    pub(crate) journal_path: PathBuf,
    /// Exact child-before-parent directory durability targets.
    pub(crate) pending_directory_targets: Arc<[PathBuf]>,
    /// Exact bytes released after one-shot inherited/creation debt succeeds.
    pub(crate) directory_charge: usize,
    /// Exact registry/path/handle accounting retained until release.
    pub(crate) registry_charge: usize,
}

impl SemanticPersistenceOwner {
    /// Starts the unique persistence worker with fixed aggregate capacity.
    pub fn new(capacity: PersistenceCapacity) -> Result<Self, PersistenceAdmissionError> {
        Self::with_backend(capacity, Arc::new(FilesystemBackend))
    }

    /// Starts one owner against an injected production-operation backend.
    #[cfg(test)]
    pub(crate) fn with_test_backend(
        capacity: PersistenceCapacity,
        backend: Arc<dyn PersistenceBackend>,
    ) -> Result<Self, PersistenceAdmissionError> {
        Self::with_backend(capacity, backend)
    }

    fn with_backend(
        capacity: PersistenceCapacity,
        backend: Arc<dyn PersistenceBackend>,
    ) -> Result<Self, PersistenceAdmissionError> {
        let baseline_bytes = capacity
            .max_frames
            .checked_mul(mem::size_of::<FrameJob>() + mem::size_of::<PersistenceFailure>())
            .and_then(|bytes| {
                capacity
                    .max_streams
                    .checked_mul(
                        mem::size_of::<StreamIdentity>()
                            + mem::size_of::<RegisteredStream>()
                            + mem::size_of::<WorkerCommand>()
                            + super::worker::touch_debt_charge()
                            + mem::size_of::<(FrameAdmissionToken, bool)>(),
                    )
                    .and_then(|registry| bytes.checked_add(registry))
            })
            .ok_or(PersistenceAdmissionError::Full)?;
        if baseline_bytes > capacity.max_bytes {
            return Err(PersistenceAdmissionError::Full);
        }
        let mut owner_epoch = NEXT_OWNER_EPOCH.load(Ordering::Relaxed);
        loop {
            let next = owner_epoch.checked_add(1).ok_or_else(|| {
                PersistenceAdmissionError::Lifecycle("owner epoch exhausted".to_owned())
            })?;
            match NEXT_OWNER_EPOCH.compare_exchange_weak(
                owner_epoch,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => owner_epoch = current,
            }
        }
        let shared = Arc::new(Shared {
            owner_epoch,
            capacity,
            backend,
            state: Mutex::new(AdmissionState {
                available: true,
                shutting_down: false,
                worker_exited: false,
                rejected_admissions_remaining: 0,
                admissions_before_rejection: 0,
                next_generation: 1,
                next_frame_token: FrameAdmissionToken::FIRST,
                last_admitted_frame: None,
                last_frame_disposition: None,
                streams: HashMap::with_capacity(capacity.max_streams),
                frames: VecDeque::with_capacity(capacity.max_frames),
                commands: VecDeque::with_capacity(capacity.max_streams),
                ledger: ResourceLedger {
                    bytes: baseline_bytes,
                    ..ResourceLedger::default()
                },
                failures: VecDeque::with_capacity(capacity.max_frames),
                capacity_pressure: None,
                capacity_full_pending: None,
                capacity_recovered: None,
                recovery_in_progress: false,
                capacity_drained: None,
            }),
            wake: Condvar::new(),
            operational_wake: Mutex::new(None),
            operational_wake_pending: AtomicBool::new(false),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("tau-semantic-persistence".to_owned())
            .spawn(move || worker_main(worker_shared))
            .map_err(|error| PersistenceAdmissionError::Lifecycle(error.to_string()))?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Drains bounded content-free asynchronous failure outcomes.
    pub fn drain_failures(&self) -> Vec<PersistenceFailure> {
        self.shared
            .operational_wake_pending
            .store(false, Ordering::Release);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.failures.drain(..).collect()
    }

    /// Installs the process-local wake used for failure observation and
    /// retained publication retry.
    ///
    /// The callback must return immediately and must not call back into this
    /// owner; it can run while an admission transition owns the state lock.
    pub fn set_operational_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self
            .shared
            .operational_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&wake));
        let pending = {
            let state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            !state.failures.is_empty()
                || state.capacity_full_pending.is_some()
                || state.capacity_recovered.is_some()
                || state.capacity_drained.is_some()
        };
        if pending {
            self.shared
                .operational_wake_pending
                .store(true, Ordering::Release);
            wake();
        }
    }

    /// Drains bounded failures and edge-triggered capacity transitions without
    /// exposing stream identities or persisted content.
    pub fn drain_operational_status(&self) -> PersistenceOperationalStatus {
        self.shared
            .operational_wake_pending
            .store(false, Ordering::Release);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        PersistenceOperationalStatus {
            usage: usage(&state.ledger),
            failures: state.failures.drain(..).collect(),
            capacity_full: state.capacity_full_pending.take(),
            recovered: state.capacity_recovered.take(),
            drained: state.capacity_drained.take(),
        }
    }

    /// Waits on the owner condition variable for one exact deterministic test
    /// outcome.
    #[cfg(test)]
    pub(crate) fn wait_for_failure_for_test(
        &self,
        kind: PersistenceFailureKind,
        timeout: Duration,
    ) -> bool {
        self.shared
            .operational_wake_pending
            .store(false, Ordering::Release);
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let (mut state, _) = self
            .shared
            .wake
            .wait_timeout_while(state, timeout, |state| {
                !state.failures.iter().any(|failure| failure.kind == kind)
            })
            .unwrap_or_else(|e| e.into_inner());
        if let Some(index) = state
            .failures
            .iter()
            .position(|failure| failure.kind == kind)
        {
            state.failures.remove(index);
            true
        } else {
            false
        }
    }

    /// Returns exact retained ledger totals for deterministic capacity tests.
    #[cfg(test)]
    pub(crate) fn ledger_for_test(&self) -> (usize, usize, usize) {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            state.ledger.frames,
            state.ledger.bytes,
            state.ledger.streams,
        )
    }

    /// Makes every generation unavailable after an unrecoverable lifecycle cut.
    pub fn fail_stop(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.available = false;
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Injects one deterministic pre-publication admission rejection.
    #[cfg(any(test, feature = "test-legacy-writer"))]
    #[doc(hidden)]
    pub fn reject_next_admission_for_test(&self) {
        self.reject_admissions_for_test(1);
    }

    /// Injects a bounded run of deterministic admission rejections.
    #[cfg(any(test, feature = "test-legacy-writer"))]
    #[doc(hidden)]
    pub fn reject_admissions_for_test(&self, count: usize) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.admissions_before_rejection = 0;
        state.rejected_admissions_remaining = count;
    }

    /// Injects rejections after a fixed number of successful admissions.
    #[cfg(any(test, feature = "test-legacy-writer"))]
    #[doc(hidden)]
    pub fn reject_admissions_after_for_test(&self, successful: usize, count: usize) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.admissions_before_rejection = successful;
        state.rejected_admissions_remaining = count;
    }

    /// Emits the same capacity-ready edge as worker disposal for deterministic
    /// harness tests.
    #[cfg(any(test, feature = "test-legacy-writer"))]
    #[doc(hidden)]
    pub fn signal_capacity_ready_for_test(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.rejected_admissions_remaining = 0;
        state.admissions_before_rejection = 0;
        report_capacity_recovered(&self.shared, &mut state);
    }

    /// Waits until every currently accepted frame is fully durable.
    #[cfg(any(test, feature = "test-legacy-writer"))]
    #[doc(hidden)]
    pub fn wait_for_latest_durability_for_test(&self, timeout: Duration) -> bool {
        let (reply, receive) = mpsc::sync_channel(1);
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.available {
                return false;
            }
            state
                .commands
                .push_back(WorkerCommand::DurabilityBarrier { reply });
        }
        self.shared.wake.notify_one();
        matches!(receive.recv_timeout(timeout), Ok(Ok(())))
    }

    /// Prepares one canonical store root on the sole mutable filesystem worker.
    pub(crate) fn prepare_root(&self, path: PathBuf) -> Result<(), PersistenceAdmissionError> {
        let (reply, receive) = mpsc::sync_channel(1);
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.available {
                return Err(PersistenceAdmissionError::Unavailable);
            }
            state
                .commands
                .push_back(WorkerCommand::PrepareRoot { path, reply });
        }
        self.shared.wake.notify_one();
        receive
            .recv()
            .unwrap_or(Err(PersistenceAdmissionError::Unavailable))
    }

    /// Reserves a new durable agent without touching its canonical path.
    ///
    /// Only its matching first `AgentStarted` frame can consume the returned
    /// lease.
    pub(crate) fn reserve_new_agent(
        &self,
        agent_id: tau_proto::AgentId,
        journal_path: PathBuf,
    ) -> Result<PersistenceLease, PersistenceAdmissionError> {
        self.register(
            StreamIdentity::Agent(agent_id),
            journal_path,
            StreamLifecycle::ReservedNewAgent,
        )
    }

    /// Prepares and strictly recovers one existing agent on the sole worker.
    pub(crate) fn prepare_existing_agent(
        &self,
        agent_id: tau_proto::AgentId,
        journal_path: PathBuf,
    ) -> Result<PreparedAgentStream, PersistenceAdmissionError> {
        let lease = self.register(
            StreamIdentity::Agent(agent_id),
            journal_path,
            StreamLifecycle::Preparing,
        )?;
        let (reply, receive) = mpsc::sync_channel(1);
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.commands.push_back(WorkerCommand::PrepareAgent {
                identity: Arc::clone(&lease.identity),
                reply,
            });
        }
        self.shared.wake.notify_one();
        match receive.recv() {
            Ok(Ok(PreparationResult::Agent(events))) => Ok(PreparedAgentStream { lease, events }),
            Ok(Ok(PreparationResult::Session { .. })) => {
                unreachable!("agent preparation returns agent records")
            }
            Ok(Err(error)) => {
                self.unregister(&lease);
                Err(error)
            }
            Err(_) => Err(PersistenceAdmissionError::Unavailable),
        }
    }

    /// Prepares both canonical session streams and manifest on the sole worker.
    pub(crate) fn prepare_session(
        &self,
        session_id: tau_proto::SessionId,
        ordinary_path: PathBuf,
        restore_path: PathBuf,
        mode: SessionPreparationMode,
    ) -> Result<PreparedSessionStreams, PersistenceAdmissionError> {
        let session_lease = self.register(
            StreamIdentity::Session(session_id.clone()),
            ordinary_path,
            StreamLifecycle::Preparing,
        )?;
        let restore_lease = match self.register(
            StreamIdentity::SessionRestore(session_id),
            restore_path,
            StreamLifecycle::Preparing,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                self.unregister(&session_lease);
                return Err(error);
            }
        };
        let (reply, receive) = mpsc::sync_channel(1);
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.commands.push_back(WorkerCommand::PrepareSession {
                session: Arc::clone(&session_lease.identity),
                restore: Arc::clone(&restore_lease.identity),
                mode,
                reply,
            });
        }
        self.shared.wake.notify_one();
        match receive.recv() {
            Ok(Ok(PreparationResult::Session {
                events,
                restore_events,
                meta,
            })) => Ok(PreparedSessionStreams {
                session_lease,
                restore_lease,
                events,
                restore_events,
                meta,
            }),
            Ok(Ok(PreparationResult::Agent(_))) => {
                unreachable!("session preparation returns session records")
            }
            Ok(Err(error)) => {
                self.unregister(&session_lease);
                self.unregister(&restore_lease);
                Err(error)
            }
            Err(_) => Err(PersistenceAdmissionError::Unavailable),
        }
    }

    /// Closes admissions and requests bounded worker-side handle release.
    pub fn release(
        &self,
        leases: &[PersistenceLease],
        timeout: Duration,
    ) -> Result<(), PersistenceAdmissionError> {
        self.release_impl(leases, timeout, true)
    }

    /// Releases handles and debts while retaining exact leases for maintenance.
    pub fn release_for_maintenance(
        &self,
        leases: &[PersistenceLease],
        timeout: Duration,
    ) -> Result<(), PersistenceAdmissionError> {
        self.release_impl(leases, timeout, false)
    }

    fn release_impl(
        &self,
        leases: &[PersistenceLease],
        timeout: Duration,
        finalize: bool,
    ) -> Result<(), PersistenceAdmissionError> {
        let identities: Vec<_> = leases
            .iter()
            .map(|lease| Arc::clone(&lease.identity))
            .collect();
        ensure_unique_identities(&identities)?;
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            for identity in &identities {
                if identity.owner_epoch != self.shared.owner_epoch
                    || !Weak::ptr_eq(&identity.shared, &Arc::downgrade(&self.shared))
                {
                    return Err(PersistenceAdmissionError::StaleLease);
                }
                let registered = state
                    .streams
                    .get(&identity.stream)
                    .filter(|stream| stream.generation == identity.generation)
                    .ok_or(PersistenceAdmissionError::StaleLease)?;
                if !matches!(
                    registered.lifecycle,
                    StreamLifecycle::ReservedNewAgent
                        | StreamLifecycle::CreationQueued
                        | StreamLifecycle::Prepared
                        | StreamLifecycle::Poisoned
                        | StreamLifecycle::CreationFailed
                ) {
                    return Err(PersistenceAdmissionError::StaleLease);
                }
            }
            for identity in &identities {
                let registered = state
                    .streams
                    .get_mut(&identity.stream)
                    .expect("release set was prevalidated");
                registered.lifecycle = StreamLifecycle::Closing;
            }
        }
        let (reply, receive) = mpsc::sync_channel(1);
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.commands.push_back(WorkerCommand::Release {
                identities: identities.clone(),
                reply,
            });
        }
        self.shared.wake.notify_one();
        receive.recv_timeout(timeout).map_err(|_| {
            PersistenceAdmissionError::Lifecycle("release deadline elapsed".to_owned())
        })??;
        if finalize {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            for identity in &identities {
                let registered = state
                    .streams
                    .get(&identity.stream)
                    .filter(|stream| {
                        stream.generation == identity.generation
                            && stream.lifecycle == StreamLifecycle::Released
                    })
                    .ok_or(PersistenceAdmissionError::StaleLease)?;
                let _ = registered;
            }
            for identity in &identities {
                let registered = state
                    .streams
                    .remove(&identity.stream)
                    .expect("finalized release was prevalidated");
                state.ledger.streams -= 1;
                state.ledger.bytes = state
                    .ledger
                    .bytes
                    .checked_sub(registered.registry_charge)
                    .expect("final release charge exists");
            }
            report_capacity_recovered(&self.shared, &mut state);
        }
        Ok(())
    }

    /// Claims read-only maintenance only after successful worker release.
    pub fn claim_maintenance(
        &self,
        leases: &[PersistenceLease],
    ) -> Result<(), PersistenceAdmissionError> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let identities: Vec<_> = leases
            .iter()
            .map(|lease| Arc::clone(&lease.identity))
            .collect();
        ensure_unique_identities(&identities)?;
        for lease in leases {
            if lease.identity.owner_epoch != self.shared.owner_epoch
                || !Weak::ptr_eq(&lease.identity.shared, &Arc::downgrade(&self.shared))
            {
                return Err(PersistenceAdmissionError::StaleLease);
            }
            let registered = state
                .streams
                .get(&lease.identity.stream)
                .filter(|stream| {
                    stream.generation == lease.identity.generation
                        && stream.lifecycle == StreamLifecycle::Released
                })
                .ok_or(PersistenceAdmissionError::StaleLease)?;
            let _ = registered;
        }
        for lease in leases {
            let registered = state
                .streams
                .get_mut(&lease.identity.stream)
                .expect("maintenance set was prevalidated");
            registered.lifecycle = StreamLifecycle::Maintenance;
        }
        Ok(())
    }

    /// Finishes exact maintenance claims and releases their registry capacity.
    pub fn finish_maintenance(
        &self,
        leases: &[PersistenceLease],
    ) -> Result<(), PersistenceAdmissionError> {
        let identities: Vec<_> = leases
            .iter()
            .map(|lease| Arc::clone(&lease.identity))
            .collect();
        ensure_unique_identities(&identities)?;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        for identity in &identities {
            if identity.owner_epoch != self.shared.owner_epoch
                || !Weak::ptr_eq(&identity.shared, &Arc::downgrade(&self.shared))
            {
                return Err(PersistenceAdmissionError::StaleLease);
            }
            state
                .streams
                .get(&identity.stream)
                .filter(|stream| {
                    stream.generation == identity.generation
                        && stream.lifecycle == StreamLifecycle::Maintenance
                })
                .ok_or(PersistenceAdmissionError::StaleLease)?;
        }
        for identity in &identities {
            let registered = state
                .streams
                .remove(&identity.stream)
                .expect("maintenance set was prevalidated");
            state.ledger.streams -= 1;
            state.ledger.bytes = state
                .ledger
                .bytes
                .checked_sub(registered.registry_charge)
                .expect("maintenance registry charge exists");
        }
        report_capacity_recovered(&self.shared, &mut state);
        Ok(())
    }

    fn register(
        &self,
        stream: StreamIdentity,
        journal_path: PathBuf,
        lifecycle: StreamLifecycle,
    ) -> Result<PersistenceLease, PersistenceAdmissionError> {
        let key_charge = stream_charge(&stream);
        let directory_targets = directory_targets(&journal_path);
        let directory_charge = directory_targets
            .iter()
            .try_fold(
                directory_targets
                    .len()
                    .checked_mul(mem::size_of::<PathBuf>())
                    .ok_or(PersistenceAdmissionError::Full)?,
                |total, path| total.checked_add(path.capacity()),
            )
            .ok_or(PersistenceAdmissionError::Full)?;
        let registry_charge = key_charge
            .checked_mul(5)
            .and_then(|charge| charge.checked_add(journal_path.capacity()))
            .and_then(|charge| charge.checked_add(directory_charge))
            .and_then(|charge| charge.checked_add(mem::size_of::<RegisteredStream>()))
            .and_then(|charge| charge.checked_add(worker_persistent_stream_charge()))
            .ok_or(PersistenceAdmissionError::Full)?;
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.available {
            return Err(PersistenceAdmissionError::Unavailable);
        }
        if state
            .streams
            .get(&stream)
            .is_some_and(|registered| registered.lifecycle == StreamLifecycle::Released)
        {
            let released = state
                .streams
                .remove(&stream)
                .expect("released registration exists");
            state.ledger.streams = state
                .ledger
                .streams
                .checked_sub(1)
                .expect("released stream permit exists");
            state.ledger.bytes = state
                .ledger
                .bytes
                .checked_sub(released.registry_charge)
                .expect("released registry charge exists");
            report_capacity_recovered(&self.shared, &mut state);
        }
        if state.streams.contains_key(&stream) {
            return Err(PersistenceAdmissionError::Full);
        }
        if state.ledger.streams >= self.shared.capacity.max_streams {
            report_capacity_full(&self.shared, &mut state, PersistenceCapacityLimit::Streams);
            return Err(PersistenceAdmissionError::Full);
        }
        if registry_charge
            > self
                .shared
                .capacity
                .max_bytes
                .saturating_sub(state.ledger.bytes)
        {
            report_capacity_full(&self.shared, &mut state, PersistenceCapacityLimit::Bytes);
            return Err(PersistenceAdmissionError::Full);
        }
        let generation = PersistenceGeneration(state.next_generation);
        state.next_generation = state.next_generation.checked_add(1).ok_or_else(|| {
            PersistenceAdmissionError::Lifecycle("stream generation exhausted".to_owned())
        })?;
        state.ledger.streams += 1;
        state.ledger.bytes += registry_charge;
        state.streams.insert(
            stream.clone(),
            RegisteredStream {
                generation,
                lifecycle,
                journal_path,
                pending_directory_targets: directory_targets.into(),
                directory_charge,
                registry_charge,
            },
        );
        Ok(PersistenceLease {
            identity: Arc::new(LeaseIdentity {
                shared: Arc::downgrade(&self.shared),
                owner_epoch: self.shared.owner_epoch,
                stream,
                generation,
            }),
        })
    }

    fn unregister(&self, lease: &PersistenceLease) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let remove = state
            .streams
            .get(&lease.identity.stream)
            .is_some_and(|registered| registered.generation == lease.identity.generation);
        if !remove {
            return;
        }
        let registered = state
            .streams
            .remove(&lease.identity.stream)
            .expect("checked registration exists");
        state.ledger.streams = state
            .ledger
            .streams
            .checked_sub(1)
            .expect("registration released once");
        state.ledger.bytes = state
            .ledger
            .bytes
            .checked_sub(registered.registry_charge)
            .expect("registry charge released once");
        report_capacity_recovered(&self.shared, &mut state);
    }
}

fn ensure_unique_identities(
    identities: &[Arc<LeaseIdentity>],
) -> Result<(), PersistenceAdmissionError> {
    for (index, identity) in identities.iter().enumerate() {
        if identities[..index]
            .iter()
            .any(|earlier| earlier.stream == identity.stream)
        {
            return Err(PersistenceAdmissionError::StaleLease);
        }
    }
    Ok(())
}

impl Drop for SemanticPersistenceOwner {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.shutting_down = true;
            state.available = false;
        }
        self.shared.wake.notify_all();
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let (state, _) = self
            .shared
            .wake
            .wait_timeout_while(state, Duration::from_millis(100), |state| {
                !state.worker_exited
            })
            .unwrap_or_else(|e| e.into_inner());
        let exited = state.worker_exited;
        drop(state);
        if exited && let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl PersistenceLease {
    /// Admits one bounded, coalescible session activity hint without blocking.
    pub(crate) fn try_touch_session(
        &self,
        last_touched: u64,
    ) -> Result<(), PersistenceAdmissionError> {
        if !matches!(self.identity.stream, StreamIdentity::Session(_)) {
            return Err(PersistenceAdmissionError::StaleLease);
        }
        let shared = self
            .identity
            .shared
            .upgrade()
            .ok_or(PersistenceAdmissionError::Unavailable)?;
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        validate_generation(
            &state,
            shared.owner_epoch,
            self.identity.owner_epoch,
            &self.identity.stream,
            self.identity.generation,
        )?;
        let prerequisite = state.last_admitted_frame;
        let prerequisite_written = prerequisite.map_or(Some(true), |prerequisite| {
            state
                .last_frame_disposition
                .filter(|(token, _)| *token == prerequisite)
                .map(|(_, written)| written)
        });
        if let Some(WorkerCommand::TouchSession {
            last_touched: retained,
            prerequisite: retained_prerequisite,
            prerequisite_written: retained_written,
            ..
        }) = state.commands.iter_mut().find(|command| {
            matches!(
                command,
                WorkerCommand::TouchSession { identity, .. }
                    if Arc::ptr_eq(identity, &self.identity)
            )
        }) {
            *retained = (*retained).max(last_touched);
            *retained_prerequisite = (*retained_prerequisite).max(prerequisite);
            if prerequisite >= *retained_prerequisite {
                *retained_written = prerequisite_written;
            }
        } else {
            if state.commands.len() >= shared.capacity.max_streams {
                return Err(PersistenceAdmissionError::Full);
            }
            state.commands.push_back(WorkerCommand::TouchSession {
                identity: Arc::clone(&self.identity),
                last_touched,
                prerequisite,
                prerequisite_written,
            });
        }
        drop(state);
        shared.wake.notify_one();
        Ok(())
    }

    /// Reserves one authoritative FIFO slot before serialized-size counting.
    pub(crate) fn try_reserve_frame(&self) -> Result<FrameReservation, PersistenceAdmissionError> {
        let shared = self
            .identity
            .shared
            .upgrade()
            .ok_or(PersistenceAdmissionError::Unavailable)?;
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        validate_generation(
            &state,
            shared.owner_epoch,
            self.identity.owner_epoch,
            &self.identity.stream,
            self.identity.generation,
        )?;
        if state.admissions_before_rejection != 0 {
            state.admissions_before_rejection -= 1;
        } else if state.rejected_admissions_remaining != 0 {
            state.rejected_admissions_remaining -= 1;
            report_capacity_full(&shared, &mut state, PersistenceCapacityLimit::Injected);
            return Err(PersistenceAdmissionError::Full);
        }
        if state.ledger.frames >= shared.capacity.max_frames {
            report_capacity_full(&shared, &mut state, PersistenceCapacityLimit::Frames);
            return Err(PersistenceAdmissionError::Full);
        }
        state.ledger.frames += 1;
        drop(state);
        Ok(FrameReservation {
            shared,
            identity: Arc::clone(&self.identity),
            transferred: false,
        })
    }
}

fn validate_generation(
    state: &AdmissionState,
    expected_owner_epoch: u64,
    owner_epoch: u64,
    stream: &StreamIdentity,
    generation: PersistenceGeneration,
) -> Result<(), PersistenceAdmissionError> {
    if owner_epoch != expected_owner_epoch {
        return Err(PersistenceAdmissionError::StaleLease);
    }
    if !state.available {
        return Err(PersistenceAdmissionError::Unavailable);
    }
    let registered = state
        .streams
        .get(stream)
        .ok_or(PersistenceAdmissionError::StaleLease)?;
    if registered.generation != generation {
        return Err(PersistenceAdmissionError::StaleLease);
    }
    match registered.lifecycle {
        StreamLifecycle::Prepared
        | StreamLifecycle::ReservedNewAgent
        | StreamLifecycle::CreationQueued => Ok(()),
        lifecycle => Err(lifecycle_admission_error(lifecycle)),
    }
}

fn lifecycle_admission_error(lifecycle: StreamLifecycle) -> PersistenceAdmissionError {
    match lifecycle {
        StreamLifecycle::Poisoned => PersistenceAdmissionError::Poisoned,
        StreamLifecycle::CreationFailed => PersistenceAdmissionError::CreationFailed,
        StreamLifecycle::Preparing => PersistenceAdmissionError::NotPrepared,
        StreamLifecycle::Closing | StreamLifecycle::Released | StreamLifecycle::Maintenance => {
            PersistenceAdmissionError::StaleLease
        }
        StreamLifecycle::Prepared
        | StreamLifecycle::ReservedNewAgent
        | StreamLifecycle::CreationQueued => {
            unreachable!("admissible lifecycle handled before error conversion")
        }
    }
}

fn stream_charge(stream: &StreamIdentity) -> usize {
    match stream {
        StreamIdentity::Agent(id) => id.as_str().len(),
        StreamIdentity::Session(id) | StreamIdentity::SessionRestore(id) => id.as_str().len(),
    }
}

fn internal_job_charge(registered: &RegisteredStream) -> usize {
    worker_internal_charge(
        registered.journal_path.capacity(),
        registered.pending_directory_targets.len(),
    )
}

fn worker_persistent_stream_charge() -> usize {
    super::worker::worker_persistent_stream_charge()
}

fn directory_targets(journal_path: &std::path::Path) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    let mut current = journal_path.parent();
    while let Some(path) = current {
        let normalized = if path.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            path.to_path_buf()
        };
        if targets.last() == Some(&normalized) {
            break;
        }
        targets.push(normalized);
        let parent = path.parent();
        if parent == Some(path) {
            break;
        }
        current = parent;
    }
    targets
}

pub(crate) fn invalidate_worker(shared: &Weak<Shared>) {
    if let Some(shared) = shared.upgrade() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.available = false;
        if !state.shutting_down {
            push_failure(
                &mut state,
                shared.capacity.max_frames,
                None,
                PersistenceFailureKind::WorkerExit,
            );
        }
        drop(state);
        shared.wake.notify_all();
        notify_operational(&shared);
    }
}

pub(crate) fn report_failure(
    shared: &Shared,
    identity: Option<Arc<LeaseIdentity>>,
    kind: PersistenceFailureKind,
) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    push_failure(&mut state, shared.capacity.max_frames, identity, kind);
    drop(state);
    shared.wake.notify_all();
    notify_operational(shared);
}

/// Records the first full edge until worker progress makes capacity available.
pub(crate) fn report_capacity_full(
    shared: &Shared,
    state: &mut AdmissionState,
    limit: PersistenceCapacityLimit,
) {
    if state.capacity_pressure.is_none() {
        let pressure = PersistenceCapacityPressure {
            limit,
            usage: usage(&state.ledger),
        };
        state.capacity_pressure = Some(pressure);
        state.capacity_full_pending = Some(pressure);
        notify_operational(shared);
    }
}

/// Records that worker disposal freed capacity after a reported full edge.
pub(crate) fn report_capacity_recovered(shared: &Shared, state: &mut AdmissionState) {
    if state.capacity_pressure.take().is_some() {
        state.capacity_recovered = Some(usage(&state.ledger));
        state.recovery_in_progress = true;
        notify_operational(shared);
    }
    if state.recovery_in_progress && state.ledger.frames == 0 {
        state.recovery_in_progress = false;
        state.capacity_drained = Some(usage(&state.ledger));
        notify_operational(shared);
    }
}

fn usage(ledger: &ResourceLedger) -> PersistenceUsage {
    PersistenceUsage {
        frames: ledger.frames,
        bytes: ledger.bytes,
        streams: ledger.streams,
    }
}

fn notify_operational(shared: &Shared) {
    if shared.operational_wake_pending.swap(true, Ordering::AcqRel) {
        return;
    }
    let wake = shared
        .operational_wake
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if let Some(wake) = wake {
        wake();
    }
}

fn push_failure(
    state: &mut AdmissionState,
    capacity: usize,
    identity: Option<Arc<LeaseIdentity>>,
    kind: PersistenceFailureKind,
) {
    if capacity == 0 {
        return;
    }
    if state.failures.len() == capacity {
        state.failures.pop_front();
    }
    state
        .failures
        .push_back(PersistenceFailure { identity, kind });
}

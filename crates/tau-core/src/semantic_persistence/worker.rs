//! Sole mutable filesystem worker and deadline scheduler.

#![expect(
    dead_code,
    reason = "store migration in this change constructs and validates frame jobs"
)]

use std::collections::{HashMap, VecDeque};
use std::fs::{File, Permissions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;

use super::backend::{ExistingPathKind, PersistenceBackend};
use super::identity::{PersistenceGeneration, StreamIdentity};
#[cfg(test)]
use super::owner::DerivedWorkPauseState;
use super::owner::{
    FrameAdmissionToken, PersistenceAdmissionError, PersistenceFailureKind, RetentionCharge,
    Shared, StagedFrame, invalidate_worker, report_failure, report_rollback_failure_and_poison,
};
use super::preparation::{
    PreparationResult, SessionPreparationMode, SessionPreparationStatus, WorkerCommand,
};

#[cfg(not(test))]
const RETRY_DELAY: Duration = Duration::from_millis(25);
#[cfg(test)]
const RETRY_DELAY: Duration = Duration::from_millis(1);
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
const COALESCE_DELAY: Duration = Duration::from_millis(10);

/// Explicit lifecycle of one registered stream generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StreamLifecycle {
    /// New agent identity exists only in memory until its first accepted frame.
    ReservedNewAgent,
    /// Matching first frame is committed and worker creation is in progress.
    CreationQueued,
    /// Worker is validating and acquiring an existing stream.
    Preparing,
    /// Worker exclusively owns its mutable filesystem state.
    Prepared,
    /// Admissions are closed while accepted work drains.
    Closing,
    /// Worker released every mutable handle.
    Released,
    /// A distinct read-only maintenance claim is active.
    Maintenance,
    /// Exact EOF rollback could not be proven.
    Poisoned,
    /// Non-destructive creation collided or terminally failed.
    CreationFailed,
}

/// One authoritative frame retained until complete, retriable, or terminal.
pub(crate) struct FrameJob {
    /// Allocation-free shared exact capability identity.
    identity: Arc<super::identity::LeaseIdentity>,
    /// Encoded CBOR payload.
    pub(crate) payload: Vec<u8>,
    /// Folded agent inputs awaiting a worker-derived journal watermark.
    checkpoint_candidate: Option<AgentCheckpointCandidate>,
    /// Optional complete checkpoint replacement payload.
    pub(crate) checkpoint: Option<Vec<u8>>,
    /// Typed semantic role proven during staging.
    kind: StagedFrameKind,
    /// Exact typed retained-resource charge.
    charge: RetentionCharge,
    /// Aggregate ledger bytes reserved for this frame and its derived debt.
    reserved_bytes: usize,
    /// Foundation-owned part retained as worker/debt state.
    internal_bytes: usize,
    /// Deadline for retrying a provably rolled-back head.
    pub(crate) retry_at: Option<Instant>,
    /// Exact complete frame watermark set only after the full payload.
    written_end: Option<u64>,
    /// Owner-local admission order for derived-work prerequisites.
    admission_watermark: FrameAdmissionToken,
}

impl FrameJob {
    /// Builds the only accounting-bearing FIFO node by consuming a reservation.
    pub(crate) fn from_reservation(
        identity: Arc<super::identity::LeaseIdentity>,
        staged: StagedFrame,
        charge: RetentionCharge,
        reserved_bytes: usize,
        internal_bytes: usize,
        admission_watermark: FrameAdmissionToken,
    ) -> Self {
        Self {
            identity,
            payload: staged.payload,
            checkpoint_candidate: staged.checkpoint,
            checkpoint: None,
            kind: staged.kind,
            charge,
            reserved_bytes,
            internal_bytes,
            retry_at: None,
            written_end: None,
            admission_watermark,
        }
    }

    /// Removes the complete off-side replacement after the atomic swap drops
    /// the superseded live projection.
    pub(crate) fn release_staging_replacement(&mut self) {
        self.reserved_bytes = self
            .reserved_bytes
            .checked_sub(self.charge.replacement)
            .expect("replacement staging charge is part of the reservation");
    }

    fn stream(&self) -> &StreamIdentity {
        &self.identity.stream
    }

    fn generation(&self) -> PersistenceGeneration {
        self.identity.generation
    }
}

/// Typed role of one staged authoritative frame.
pub(crate) enum StagedFrameKind {
    /// Any ordinary frame for a prepared or accepted-creation generation.
    Ordinary,
    /// Exact matching sequence-zero AgentStarted proof.
    FirstAgent(FirstAgentStartProof),
}

/// Unforgeable proof emitted only by semantic record validation.
pub(crate) struct FirstAgentStartProof {
    /// Private marker prevents boolean-like caller assertions.
    _private: (),
}

/// Folded agent checkpoint inputs whose filesystem watermark remains
/// worker-owned.
pub(crate) struct AgentCheckpointCandidate {
    /// Exact canonical agent identity.
    pub(crate) agent_id: tau_proto::AgentId,
    /// Complete folded summary through this frame.
    pub(crate) summary: crate::AgentSummary,
    /// Sequence expected immediately after this frame.
    pub(crate) next_seq: crate::PersistedAgentEventSeq,
}

impl StagedFrame {
    /// Builds an ordinary staged frame after off-side encoding.
    pub(crate) fn ordinary(payload: Vec<u8>, checkpoint: Option<AgentCheckpointCandidate>) -> Self {
        Self {
            payload,
            checkpoint,
            kind: StagedFrameKind::Ordinary,
        }
    }

    /// Builds the typed first-agent frame only for a matching sequence-zero
    /// fact.
    pub(crate) fn first_agent(
        lease: &super::identity::PersistenceLease,
        record: &crate::PersistedAgentEvent,
        payload: Vec<u8>,
        checkpoint: Option<AgentCheckpointCandidate>,
    ) -> Result<Self, PersistenceAdmissionError> {
        let StreamIdentity::Agent(agent_id) = lease.stream() else {
            return Err(PersistenceAdmissionError::StaleLease);
        };
        if record.seq.get() != 0
            || !matches!(
                &record.event,
                tau_proto::Event::AgentStarted(started) if &started.agent_id == agent_id
            )
        {
            return Err(PersistenceAdmissionError::NotPrepared);
        }
        Ok(Self {
            payload,
            checkpoint,
            kind: StagedFrameKind::FirstAgent(FirstAgentStartProof { _private: () }),
        })
    }

    /// Verifies actual vector capacities fit their typed caller reservation.
    pub(crate) fn validate_charge(
        &self,
        charge: RetentionCharge,
    ) -> Result<(), PersistenceAdmissionError> {
        let frame = self.payload.capacity().saturating_add(8);
        let checkpoint = if self.checkpoint.is_some() {
            MAX_CHECKPOINT_BYTES
        } else {
            0
        };
        if frame > charge.frame || checkpoint > charge.checkpoint {
            return Err(PersistenceAdmissionError::Full);
        }
        Ok(())
    }
}

/// Files and offsets owned only by the worker after preparation.
struct PreparedStream {
    /// Exact generation guarding this handle set.
    generation: PersistenceGeneration,
    /// Open journal append handle.
    journal: File,
    /// Exclusive advisory lock retained for the generation.
    _lock: File,
    /// Exact append offset last proven complete.
    offset: u64,
    /// Prepared canonical manifest authority for an ordinary session.
    session_meta: Option<crate::SessionMeta>,
}

/// Restartable non-destructive creation owned only by the worker.
#[derive(Default)]
struct NewAgentCreation {
    /// This generation successfully created the directory.
    directory_owned: bool,
    /// The owned directory is verified owner-private.
    directory_private: bool,
    /// This generation successfully created and locked the lock file.
    lock: Option<File>,
    /// The retained created lock handle acquired its exclusive flock.
    lock_acquired: bool,
    /// This generation successfully created the journal.
    journal: Option<File>,
}

/// Coalesced derived work keyed by exact stream generation.
struct DurabilityDebt {
    /// Allocation-free shared exact stream capability.
    identity: Arc<super::identity::LeaseIdentity>,
    /// Newest eligible checkpoint payload.
    checkpoint: Option<Vec<u8>>,
    /// Bytes transferred from frame admission into this debt.
    charge: usize,
    /// Exact checkpoint vector capacity retained within `charge`.
    checkpoint_charge: usize,
    /// Foundation-owned sync/debt state retained within `charge`.
    internal_charge: usize,
    /// Exact child-before-parent directory debt captured when admitted.
    directory_targets: Arc<[std::path::PathBuf]>,
    /// Highest complete frame covered by this debt.
    end_offset: u64,
    /// Earliest next retry.
    retry_at: Instant,
}

/// Lossy, coalesced session activity debt.
struct TouchDebt {
    /// Exact ordinary-session generation.
    identity: Arc<super::identity::LeaseIdentity>,
    /// Latest admitted activity hint.
    last_touched: u64,
    /// Earliest retry; later hints do not reset failed backoff.
    retry_at: Instant,
    /// Highest authoritative frame that must be written first.
    prerequisite: Option<FrameAdmissionToken>,
    /// None until exact FIFO disposition; false terminally drops this lossy
    /// hint.
    prerequisite_written: Option<bool>,
}

enum AppendDisposition {
    Complete,
    Retry,
    Terminal,
    WorkerExit,
}

/// Outcome of one complete length-prefixed frame write.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum FrameAppendError {
    /// The journal position could not be read before writing.
    Open,
    /// The worker was asked to exit at the write boundary.
    WorkerExit,
    /// A failed write was rolled back to the original EOF.
    RolledBack,
    /// A failed write could not be rolled back to the original EOF.
    RollbackFailed,
}

struct WorkerExitGuard {
    /// Weak state avoids extending owner lifetime solely for invalidation.
    shared: Weak<Shared>,
}

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        invalidate_worker(&self.shared);
        if let Some(shared) = self.shared.upgrade() {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.worker_exited = true;
            drop(state);
            shared.wake.notify_all();
        }
    }
}

pub(crate) fn worker_main(shared: Arc<Shared>) {
    let _exit_guard = WorkerExitGuard {
        shared: Arc::downgrade(&shared),
    };
    let mut streams = HashMap::<StreamIdentity, PreparedStream>::new();
    let mut creations = HashMap::<StreamIdentity, NewAgentCreation>::new();
    let mut debts = VecDeque::<DurabilityDebt>::new();
    let mut touches = VecDeque::<TouchDebt>::new();
    let mut head: Option<FrameJob> = None;
    let mut durability_barrier = None;
    let mut derived_since_frame = false;
    loop {
        #[cfg(test)]
        pause_before_due_derived_work_for_test(&shared, &head, &debts, &touches);
        if durability_barrier.is_none() {
            let command = {
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                (state.available
                    && head.is_none()
                    && state.frames.is_empty()
                    && state
                        .commands
                        .iter()
                        .any(|command| matches!(command, WorkerCommand::Release { .. })))
                .then(|| state.commands.pop_front())
                .flatten()
            };
            if let Some(command) = command {
                process_command(
                    &shared,
                    &mut streams,
                    &mut debts,
                    &mut touches,
                    &mut durability_barrier,
                    command,
                );
                continue;
            }
        }
        service_one_due_debt(&shared, &streams, &mut debts);
        service_one_due_touch(&shared, &mut streams, &mut touches);
        if durability_barrier.is_some() {
            if debts.is_empty() {
                let reply: SyncSender<Result<(), PersistenceAdmissionError>> =
                    durability_barrier.take().expect("barrier exists");
                let _ = reply.send(Ok(()));
            } else {
                let deadline = debts
                    .iter()
                    .map(|debt| debt.retry_at)
                    .min()
                    .expect("pending debt has deadline");
                let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                let timeout = deadline.saturating_duration_since(Instant::now());
                let _ = shared
                    .wake
                    .wait_timeout(state, timeout)
                    .unwrap_or_else(|e| e.into_inner());
                continue;
            }
        }
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let retry_deadline = minimum_deadline(head.as_ref(), &debts, &touches);
        while state.available
            && head.is_none()
            && state.frames.is_empty()
            && state.commands.is_empty()
        {
            match retry_deadline {
                Some(deadline) => {
                    let timeout = deadline.saturating_duration_since(Instant::now());
                    if timeout.is_zero() {
                        break;
                    }
                    let (next, _) = shared
                        .wake
                        .wait_timeout(state, timeout)
                        .unwrap_or_else(|e| e.into_inner());
                    state = next;
                    break;
                }
                None => {
                    state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
        if !state.available && head.is_none() && state.frames.is_empty() {
            return;
        }
        if !derived_since_frame
            && (head.is_some() || !state.frames.is_empty())
            && let Some(index) = state
                .commands
                .iter()
                .position(|command| matches!(command, WorkerCommand::TouchSession { .. }))
        {
            let command = state
                .commands
                .remove(index)
                .expect("located touch command exists");
            drop(state);
            process_command(
                &shared,
                &mut streams,
                &mut debts,
                &mut touches,
                &mut durability_barrier,
                command,
            );
            derived_since_frame = true;
            continue;
        }
        if head.is_none()
            && state.frames.is_empty()
            && let Some(command) = state.commands.pop_front()
        {
            drop(state);
            process_command(
                &shared,
                &mut streams,
                &mut debts,
                &mut touches,
                &mut durability_barrier,
                command,
            );
            continue;
        }
        if head.is_none() {
            head = state.frames.pop_front();
        }
        if head
            .as_ref()
            .and_then(|job| job.retry_at)
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            let deadline = head.as_ref().and_then(|job| job.retry_at);
            let timeout = deadline
                .expect("checked retry deadline exists")
                .saturating_duration_since(Instant::now());
            let (next, _) = shared
                .wake
                .wait_timeout(state, timeout)
                .unwrap_or_else(|e| e.into_inner());
            drop(next);
            continue;
        }
        drop(state);

        let Some(job) = head.as_mut() else {
            continue;
        };
        derived_since_frame = false;
        match append_job(&shared, &mut streams, &mut creations, job) {
            AppendDisposition::Complete => {
                let completed = head.take().expect("head exists");
                record_frame_disposition(
                    &shared,
                    &mut touches,
                    completed.admission_watermark,
                    true,
                );
                transfer_or_release(&shared, completed, &mut debts);
            }
            AppendDisposition::Terminal => {
                let completed = head.take().expect("head exists");
                record_frame_disposition(
                    &shared,
                    &mut touches,
                    completed.admission_watermark,
                    false,
                );
                release_job(&shared, &completed);
            }
            AppendDisposition::Retry => {
                job.retry_at = Some(Instant::now() + RETRY_DELAY);
            }
            AppendDisposition::WorkerExit => return,
        }
    }
}

#[cfg(test)]
fn pause_before_due_derived_work_for_test(
    shared: &Shared,
    head: &Option<FrameJob>,
    debts: &VecDeque<DurabilityDebt>,
    touches: &VecDeque<TouchDebt>,
) {
    if minimum_deadline(head.as_ref(), debts, touches)
        .is_none_or(|deadline| deadline > Instant::now())
    {
        return;
    }
    let mut state = shared
        .derived_work_pause
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !state.armed {
        return;
    }
    state.reached = true;
    shared.derived_work_pause.wake.notify_all();
    while !state.released {
        state = shared
            .derived_work_pause
            .wake
            .wait(state)
            .unwrap_or_else(|error| error.into_inner());
    }
    *state = DerivedWorkPauseState::default();
}

fn record_frame_disposition(
    shared: &Shared,
    touches: &mut VecDeque<TouchDebt>,
    token: FrameAdmissionToken,
    written: bool,
) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.last_frame_disposition = Some((token, written));
    for command in &mut state.commands {
        if let WorkerCommand::TouchSession {
            prerequisite,
            prerequisite_written,
            ..
        } = command
            && *prerequisite == Some(token)
        {
            *prerequisite_written = Some(written);
        }
    }
    drop(state);
    for debt in touches {
        if debt.prerequisite == Some(token) {
            debt.prerequisite_written = Some(written);
        }
    }
}

fn process_command(
    shared: &Shared,
    streams: &mut HashMap<StreamIdentity, PreparedStream>,
    debts: &mut VecDeque<DurabilityDebt>,
    touches: &mut VecDeque<TouchDebt>,
    _durability_barrier: &mut Option<SyncSender<Result<(), PersistenceAdmissionError>>>,
    command: WorkerCommand,
) {
    match command {
        WorkerCommand::PrepareRoot { path, reply } => {
            let result = shared
                .backend
                .create_owner_directories(&path)
                .and_then(|()| {
                    let mut target = Some(path.as_path());
                    while let Some(directory_path) = target {
                        let directory = shared.backend.open_directory(directory_path)?;
                        shared.backend.sync_all(&directory)?;
                        target = directory_path.parent();
                    }
                    Ok(())
                })
                .map_err(|error| PersistenceAdmissionError::Lifecycle(error.to_string()));
            let _ = reply.send(result);
        }
        WorkerCommand::PrepareAgent { identity, reply } => {
            let result = prepare_agent(shared, streams, &identity)
                .map(PreparationResult::Agent)
                .map_err(preparation_error);
            if result.is_ok() {
                set_lifecycle(
                    shared,
                    &identity.stream,
                    identity.generation,
                    StreamLifecycle::Prepared,
                );
            }
            let _ = reply.send(result);
        }
        WorkerCommand::PrepareSession {
            session,
            restore,
            mode,
            reply,
        } => {
            let result = prepare_session(shared, streams, &session, &restore, mode)
                .map(
                    |(events, restore_events, meta, status)| PreparationResult::Session {
                        events,
                        restore_events,
                        meta,
                        status,
                    },
                )
                .map_err(preparation_error);
            if result.is_ok() {
                set_lifecycle(
                    shared,
                    &session.stream,
                    session.generation,
                    StreamLifecycle::Prepared,
                );
                set_lifecycle(
                    shared,
                    &restore.stream,
                    restore.generation,
                    StreamLifecycle::Prepared,
                );
            }
            let _ = reply.send(result);
        }
        WorkerCommand::TouchSession {
            identity,
            last_touched,
            prerequisite,
            prerequisite_written,
        } => {
            if let Some(existing) = touches
                .iter_mut()
                .find(|debt| Arc::ptr_eq(&debt.identity, &identity))
            {
                existing.last_touched = existing.last_touched.max(last_touched);
                existing.prerequisite = existing.prerequisite.max(prerequisite);
                if prerequisite >= existing.prerequisite {
                    existing.prerequisite_written = prerequisite_written;
                }
            } else {
                touches.push_back(TouchDebt {
                    identity,
                    last_touched,
                    retry_at: Instant::now() + COALESCE_DELAY,
                    prerequisite,
                    prerequisite_written,
                });
            }
        }
        WorkerCommand::Release { identities, reply } => {
            let result = flush_release_debts(shared, streams, debts, &identities);
            if let Err(error) = result {
                let _ = reply.send(Err(PersistenceAdmissionError::Lifecycle(error.to_string())));
                return;
            }
            touches.retain(|debt| {
                !identities
                    .iter()
                    .any(|identity| Arc::ptr_eq(identity, &debt.identity))
            });
            for identity in &identities {
                streams.remove(&identity.stream);
                set_lifecycle(
                    shared,
                    &identity.stream,
                    identity.generation,
                    StreamLifecycle::Released,
                );
            }
            let _ = reply.send(Ok(()));
        }
        #[cfg(any(test, feature = "test-legacy-writer"))]
        WorkerCommand::DurabilityBarrier { reply } => {
            *_durability_barrier = Some(reply);
        }
    }
}

fn preparation_error(error: io::Error) -> PersistenceAdmissionError {
    if error.kind() == io::ErrorKind::NotFound {
        PersistenceAdmissionError::StreamNotFound
    } else {
        PersistenceAdmissionError::Lifecycle(error.to_string())
    }
}

fn prepare_agent(
    shared: &Shared,
    streams: &mut HashMap<StreamIdentity, PreparedStream>,
    identity: &super::identity::LeaseIdentity,
) -> io::Result<Vec<crate::PersistedAgentEvent>> {
    let StreamIdentity::Agent(agent_id) = &identity.stream else {
        return Err(io::Error::other("agent preparation used non-agent stream"));
    };
    let journal_path = registered_path(shared, identity)?;
    let directory = journal_path
        .parent()
        .ok_or_else(|| io::Error::other("agent journal has no parent"))?;
    let lock = shared.backend.open_existing_file(&directory.join("lock"))?;
    shared.backend.try_lock(&lock)?;
    let mut journal = shared.backend.open_existing_file(&journal_path)?;
    let events =
        recover_records::<crate::PersistedAgentEvent>(shared, &journal, TornTailPolicy::Repair)?;
    crate::AgentTree::try_from_events(agent_id.clone(), &events)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let offset = shared.backend.seek_end(&mut journal)?;
    streams.insert(
        identity.stream.clone(),
        PreparedStream {
            generation: identity.generation,
            journal,
            _lock: lock,
            offset,
            session_meta: None,
        },
    );
    Ok(events)
}

fn prepare_session(
    shared: &Shared,
    streams: &mut HashMap<StreamIdentity, PreparedStream>,
    session_identity: &super::identity::LeaseIdentity,
    restore_identity: &super::identity::LeaseIdentity,
    mode: SessionPreparationMode,
) -> io::Result<(
    Vec<crate::PersistedSessionEvent>,
    Vec<crate::PersistedSessionEvent>,
    crate::SessionMeta,
    super::preparation::SessionPreparationStatus,
)> {
    let StreamIdentity::Session(session_id) = &session_identity.stream else {
        return Err(io::Error::other("ordinary preparation used wrong stream"));
    };
    let ordinary_path = registered_path(shared, session_identity)?;
    let restore_path = registered_path(shared, restore_identity)?;
    let directory = ordinary_path
        .parent()
        .ok_or_else(|| io::Error::other("session journal has no parent"))?;
    let meta_path = directory.join("meta.json");
    let lock_path = directory.join("lock");
    let exclusive_created = if matches!(
        mode,
        SessionPreparationMode::Create | SessionPreparationMode::CreateOrResume
    ) {
        match shared.backend.create_owner_directory(directory) {
            Ok(()) => Some(initialize_owned_session(shared, directory, &lock_path)?),
            Err(error)
                if matches!(mode, SessionPreparationMode::CreateOrResume)
                    && error.kind() == io::ErrorKind::AlreadyExists =>
            {
                if shared.backend.existing_path_kind(directory)? != ExistingPathKind::Directory {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "existing session path is not a real directory: {}",
                            directory.display()
                        ),
                    ));
                }
                None
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("session directory already exists: {}", directory.display()),
                ));
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let (lock, meta, creating) = match exclusive_created {
        Some(created) => created,
        None => match open_existing_session_artifact(shared, &lock_path, mode) {
            Ok(lock) => {
                shared.backend.try_lock(&lock)?;
                let bytes = if matches!(mode, SessionPreparationMode::CreateOrResume) {
                    let meta_file = shared
                        .backend
                        .open_existing_regular_file_read_no_follow(&meta_path)?;
                    shared.backend.read_open_file(&meta_file)?
                } else {
                    shared.backend.read_file(&meta_path)?
                };
                let meta = serde_json::from_slice::<crate::SessionMeta>(&bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                (lock, meta, false)
            }
            Err(error)
                if matches!(mode, SessionPreparationMode::New)
                    && error.kind() == io::ErrorKind::NotFound =>
            {
                for canonical in [&meta_path, &ordinary_path, &restore_path] {
                    match shared.backend.read_file(canonical) {
                        Ok(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                format!(
                                    "new session canonical artifact already exists: {}",
                                    canonical.display()
                                ),
                            ));
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                if let Err(error) = shared.backend.create_owner_directory(directory)
                    && error.kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "create new session directory {}: {error}",
                            directory.display()
                        ),
                    ));
                }
                initialize_owned_session(shared, directory, &lock_path)?
            }
            Err(error) => return Err(error),
        },
    };
    let mut ordinary = match creating {
        true => shared.backend.create_new_file(&ordinary_path)?,
        false => open_existing_session_artifact(shared, &ordinary_path, mode)?,
    };
    let mut restore = match creating {
        true => shared.backend.create_new_file(&restore_path)?,
        false => open_existing_session_artifact(shared, &restore_path, mode)?,
    };
    if creating {
        write_manifest(shared, &meta_path, &meta)?;
    }
    let torn_tail_policy = if !creating && matches!(mode, SessionPreparationMode::CreateOrResume) {
        TornTailPolicy::Reject
    } else {
        TornTailPolicy::Repair
    };
    let events =
        recover_records::<crate::PersistedSessionEvent>(shared, &ordinary, torn_tail_policy)?;
    crate::SessionMembership::try_from_events(session_id.clone(), &events)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let restore_events =
        recover_records::<crate::PersistedSessionEvent>(shared, &restore, torn_tail_policy)?;
    validate_restore_records(&restore_events)?;
    let ordinary_offset = shared.backend.seek_end(&mut ordinary)?;
    let restore_offset = shared.backend.seek_end(&mut restore)?;
    streams.insert(
        session_identity.stream.clone(),
        PreparedStream {
            generation: session_identity.generation,
            journal: ordinary,
            _lock: lock,
            offset: ordinary_offset,
            session_meta: Some(meta.clone()),
        },
    );
    // The ordinary prepared stream owns the exclusive session lock. The restore
    // stream needs no second mutable lock handle because both release together.
    let restore_lock = open_existing_session_artifact(shared, &lock_path, mode)?;
    streams.insert(
        restore_identity.stream.clone(),
        PreparedStream {
            generation: restore_identity.generation,
            journal: restore,
            _lock: restore_lock,
            offset: restore_offset,
            session_meta: None,
        },
    );
    let status = if creating {
        SessionPreparationStatus::Created
    } else {
        SessionPreparationStatus::Resumed
    };
    Ok((events, restore_events, meta, status))
}

/// Initializes canonical lock and manifest authority after claiming a
/// directory.
fn initialize_owned_session(
    shared: &Shared,
    directory: &Path,
    lock_path: &Path,
) -> io::Result<(File, crate::SessionMeta, bool)> {
    #[cfg(unix)]
    shared
        .backend
        .set_permissions(directory, Permissions::from_mode(0o700))?;
    let now = unix_seconds();
    let pending_lock_path = lock_path.with_extension("pending");
    let lock = shared.backend.create_new_file(&pending_lock_path)?;
    shared.backend.try_lock(&lock)?;
    if let Err(publication_error) = shared
        .backend
        .publish_no_replace(&pending_lock_path, lock_path)
    {
        return match shared.backend.remove_file(&pending_lock_path) {
            Ok(()) => Err(publication_error),
            Err(cleanup_error) => Err(io::Error::new(
                publication_error.kind(),
                format!(
                    "{publication_error}; also failed to remove unpublished pending lock {}: \
                     {cleanup_error}",
                    pending_lock_path.display()
                ),
            )),
        };
    }
    // The canonical hard link now owns lock authority. Cleanup failure may leave
    // an inert alias, but it must not report that committed publication failed.
    let _ = shared.backend.remove_file(&pending_lock_path);
    Ok((
        lock,
        crate::SessionMeta {
            created_at: now,
            last_touched: now,
        },
        true,
    ))
}

/// Policy for an incomplete final length prefix or payload.
#[derive(Clone, Copy)]
enum TornTailPolicy {
    /// Preserve existing recovery semantics by removing the incomplete suffix.
    Repair,
    /// Reject the journal without changing any durable byte.
    Reject,
}

fn recover_records<T: DeserializeOwned>(
    shared: &Shared,
    file: &File,
    torn_tail_policy: TornTailPolicy,
) -> io::Result<Vec<T>> {
    let bytes = shared.backend.read_open_file(file)?;
    let mut cursor = 0usize;
    let mut records = Vec::new();
    while cursor < bytes.len() {
        let frame_start = cursor;
        if bytes.len() - cursor < 8 {
            match torn_tail_policy {
                TornTailPolicy::Repair => {
                    shared.backend.truncate(file, frame_start as u64)?;
                    shared.backend.sync_data(file)?;
                    break;
                }
                TornTailPolicy::Reject => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "semantic journal ends with an incomplete frame header",
                    ));
                }
            }
        }
        let length = u64::from_le_bytes(
            bytes[cursor..cursor + 8]
                .try_into()
                .expect("exact prefix slice"),
        );
        cursor += 8;
        if length > 64 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "semantic journal frame exceeds 64 MiB",
            ));
        }
        let length = usize::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame length overflow"))?;
        if bytes.len() - cursor < length {
            match torn_tail_policy {
                TornTailPolicy::Repair => {
                    shared.backend.truncate(file, frame_start as u64)?;
                    shared.backend.sync_data(file)?;
                    break;
                }
                TornTailPolicy::Reject => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "semantic journal ends with an incomplete frame payload",
                    ));
                }
            }
        }
        let record = ciborium::from_reader(&bytes[cursor..cursor + length])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        records.push(record);
        cursor += length;
    }
    Ok(records)
}

fn open_existing_session_artifact(
    shared: &Shared,
    path: &Path,
    mode: SessionPreparationMode,
) -> io::Result<File> {
    if matches!(mode, SessionPreparationMode::CreateOrResume) {
        shared
            .backend
            .open_existing_regular_file_write_no_follow(path)
    } else {
        shared.backend.open_existing_file(path)
    }
}

fn validate_restore_records(records: &[crate::PersistedSessionEvent]) -> io::Result<()> {
    let mut expected = crate::PersistedSessionEventSeq::new(0);
    for record in records {
        if record.seq != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "restore journal sequence mismatch",
            ));
        }
        crate::session_store::validate_restore_event(&record.event)
            .map_err(|error| io::Error::other(error.to_string()))?;
        expected = expected.next();
    }
    Ok(())
}

fn registered_path(
    shared: &Shared,
    identity: &super::identity::LeaseIdentity,
) -> io::Result<std::path::PathBuf> {
    let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state
        .streams
        .get(&identity.stream)
        .filter(|registered| registered.generation == identity.generation)
        .map(|registered| registered.journal_path.clone())
        .ok_or_else(|| io::Error::other("stale preparation generation"))
}

fn write_manifest(
    shared: &Shared,
    path: &std::path::Path,
    meta: &crate::SessionMeta,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(meta)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temporary = path.with_extension("json.tmp");
    let mut file = shared.backend.create_temporary_file(&temporary)?;
    shared.backend.write_all(&mut file, &bytes)?;
    shared.backend.sync_all(&file)?;
    shared.backend.rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        let directory = shared.backend.open_directory(parent)?;
        shared.backend.sync_all(&directory)?;
    }
    Ok(())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn append_job(
    shared: &Shared,
    streams: &mut HashMap<StreamIdentity, PreparedStream>,
    creations: &mut HashMap<StreamIdentity, NewAgentCreation>,
    job: &mut FrameJob,
) -> AppendDisposition {
    let lifecycle = {
        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .streams
            .get(job.stream())
            .filter(|registered| registered.generation == job.generation())
            .map(|registered| registered.lifecycle)
    };
    match lifecycle {
        Some(StreamLifecycle::Poisoned | StreamLifecycle::CreationFailed) | None => {
            report_failure(
                shared,
                Some(Arc::clone(&job.identity)),
                PersistenceFailureKind::GenerationFailed,
            );
            return AppendDisposition::Terminal;
        }
        Some(lifecycle @ (StreamLifecycle::CreationQueued | StreamLifecycle::Closing))
            if !streams.contains_key(job.stream())
                && (lifecycle == StreamLifecycle::CreationQueued
                    || matches!(job.kind, StagedFrameKind::FirstAgent(_))) =>
        {
            match advance_creation(shared, creations, job) {
                Ok(Some(stream)) => {
                    streams.insert(job.stream().clone(), stream);
                    creations.remove(job.stream());
                    set_lifecycle(
                        shared,
                        job.stream(),
                        job.generation(),
                        StreamLifecycle::Prepared,
                    );
                }
                Ok(None) => return AppendDisposition::Retry,
                Err(CreationError::Collision) => {
                    report_failure(
                        shared,
                        Some(Arc::clone(&job.identity)),
                        PersistenceFailureKind::Collision,
                    );
                    set_lifecycle(
                        shared,
                        job.stream(),
                        job.generation(),
                        StreamLifecycle::CreationFailed,
                    );
                    creations.remove(job.stream());
                    return AppendDisposition::Terminal;
                }
                Err(CreationError::Retry(kind)) => {
                    report_failure(shared, Some(Arc::clone(&job.identity)), kind);
                    return AppendDisposition::Retry;
                }
            }
        }
        Some(
            StreamLifecycle::Prepared | StreamLifecycle::CreationQueued | StreamLifecycle::Closing,
        ) => {}
        Some(_) => return AppendDisposition::Terminal,
    }
    let Some(stream) = streams.get_mut(job.stream()) else {
        return AppendDisposition::Retry;
    };
    if stream.generation != job.generation() {
        return AppendDisposition::Terminal;
    }
    let end = match append_frame(
        shared.backend.as_ref(),
        &mut stream.journal,
        &job.payload,
        || {
            report_failure(
                shared,
                Some(Arc::clone(&job.identity)),
                PersistenceFailureKind::Write,
            );
        },
    ) {
        Ok(end) => end,
        Err(FrameAppendError::Open) => {
            report_failure(
                shared,
                Some(Arc::clone(&job.identity)),
                PersistenceFailureKind::Open,
            );
            return AppendDisposition::Retry;
        }
        Err(FrameAppendError::WorkerExit) => return AppendDisposition::WorkerExit,
        Err(FrameAppendError::RolledBack) => return AppendDisposition::Retry,
        Err(FrameAppendError::RollbackFailed) => {
            report_rollback_failure_and_poison(shared, Arc::clone(&job.identity));
            streams.remove(job.stream());
            return AppendDisposition::Terminal;
        }
    };
    stream.offset = end;
    job.written_end = Some(stream.offset);
    if let Some(candidate) = job.checkpoint_candidate.take() {
        let checkpoint = shared
            .backend
            .journal_position(&stream.journal, stream.offset)
            .and_then(|position| {
                let checkpoint = crate::AgentCheckpoint::new(
                    candidate.agent_id,
                    candidate.summary,
                    candidate.next_seq,
                    &position,
                );
                serde_json::to_vec_pretty(&checkpoint)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            });
        match checkpoint {
            Ok(bytes) if bytes.len() <= MAX_CHECKPOINT_BYTES => job.checkpoint = Some(bytes),
            Ok(_) | Err(_) => report_failure(
                shared,
                Some(Arc::clone(&job.identity)),
                PersistenceFailureKind::Sync,
            ),
        }
    }
    AppendDisposition::Complete
}

enum CreationError {
    Collision,
    Retry(PersistenceFailureKind),
}

fn advance_creation(
    shared: &Shared,
    creations: &mut HashMap<StreamIdentity, NewAgentCreation>,
    job: &FrameJob,
) -> Result<Option<PreparedStream>, CreationError> {
    let path = {
        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .streams
            .get(job.stream())
            .filter(|stream| stream.generation == job.generation())
            .map(|stream| stream.journal_path.clone())
            .ok_or(CreationError::Collision)?
    };
    let directory = path.parent().ok_or(CreationError::Collision)?;
    let agent_id = match job.stream() {
        StreamIdentity::Agent(agent_id) => agent_id,
        _ => return Err(CreationError::Collision),
    };
    let agents_dir = directory.parent().ok_or(CreationError::Collision)?;
    let tombstone = crate::retired_agent_tombstone(agents_dir, agent_id);
    match shared.backend.existing_path_kind(&tombstone) {
        Ok(_) => return Err(CreationError::Collision),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(CreationError::Retry(PersistenceFailureKind::Open)),
    }
    let creation = creations.entry(job.stream().clone()).or_default();
    if !creation.directory_owned {
        match shared.backend.create_owner_directory(directory) {
            Ok(()) => {
                creation.directory_owned = true;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CreationError::Collision);
            }
            Err(_) => return Err(CreationError::Retry(PersistenceFailureKind::Open)),
        }
    }
    if !creation.directory_private {
        #[cfg(unix)]
        {
            shared
                .backend
                .set_permissions(directory, Permissions::from_mode(0o700))
                .map_err(|_| CreationError::Retry(PersistenceFailureKind::Open))?;
        }
        creation.directory_private = true;
    }
    if creation.lock.is_none() {
        match shared.backend.create_new_file(&directory.join("lock")) {
            Ok(lock) => {
                creation.lock = Some(lock);
                shared
                    .backend
                    .try_lock(
                        creation
                            .lock
                            .as_ref()
                            .expect("created lock is retained before locking"),
                    )
                    .map_err(|_| CreationError::Retry(PersistenceFailureKind::Lock))?;
                creation.lock_acquired = true;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CreationError::Collision);
            }
            Err(_) => return Err(CreationError::Retry(PersistenceFailureKind::Open)),
        }
    }
    if !creation.lock_acquired {
        shared
            .backend
            .try_lock(
                creation
                    .lock
                    .as_ref()
                    .expect("created lock handle remains retained"),
            )
            .map_err(|_| CreationError::Retry(PersistenceFailureKind::Lock))?;
        creation.lock_acquired = true;
    }
    if creation.journal.is_none() {
        match shared.backend.create_new_file(&path) {
            Ok(journal) => creation.journal = Some(journal),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CreationError::Collision);
            }
            Err(_) => return Err(CreationError::Retry(PersistenceFailureKind::Open)),
        }
    }
    let journal = creation
        .journal
        .take()
        .expect("completed creation owns journal");
    let lock = creation.lock.take().expect("completed creation owns lock");
    Ok(Some(PreparedStream {
        generation: job.generation(),
        journal,
        _lock: lock,
        offset: 0,
        session_meta: None,
    }))
}

fn transfer_or_release(shared: &Shared, mut job: FrameJob, debts: &mut VecDeque<DurabilityDebt>) {
    let job_end_offset = job
        .written_end
        .expect("complete frame transfers its exact watermark");
    let checkpoint_charge = job
        .checkpoint
        .as_ref()
        .map_or(0, Vec::capacity)
        .min(job.charge.checkpoint);
    let internal_charge = job.internal_bytes;
    let directory_targets = debt_directory_targets(shared, &job);
    let debt_charge = checkpoint_charge
        .saturating_add(internal_charge)
        .min(job.reserved_bytes);
    let released_bytes = job.reserved_bytes - debt_charge;
    {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ledger.frames = state
            .ledger
            .frames
            .checked_sub(1)
            .expect("completed frame releases count exactly once");
        state.ledger.bytes = state
            .ledger
            .bytes
            .checked_sub(released_bytes)
            .expect("completed frame releases non-debt bytes exactly once");
        super::owner::report_capacity_recovered(shared, &mut state);
    }
    let key = (job.stream(), job.generation());
    if let Some(existing) = debts
        .iter_mut()
        .find(|debt| (&debt.identity.stream, debt.identity.generation) == key)
    {
        let old_charge = existing.charge;
        let new_checkpoint = job.checkpoint.take();
        if let Some(new_checkpoint) = new_checkpoint {
            existing.checkpoint = Some(new_checkpoint);
            existing.checkpoint_charge = checkpoint_charge;
        }
        if existing.directory_targets.is_empty() && !directory_targets.is_empty() {
            existing.directory_targets = directory_targets;
        }
        existing.end_offset = existing.end_offset.max(job_end_offset);
        existing.internal_charge = internal_charge;
        existing.charge = existing
            .checkpoint_charge
            .saturating_add(existing.internal_charge);
        let combined = old_charge.saturating_add(debt_charge);
        release_debt_bytes(shared, combined.saturating_sub(existing.charge));
    } else {
        debts.push_back(DurabilityDebt {
            identity: Arc::clone(&job.identity),
            checkpoint: job.checkpoint,
            charge: debt_charge,
            checkpoint_charge,
            internal_charge,
            directory_targets,
            end_offset: job_end_offset,
            retry_at: Instant::now() + COALESCE_DELAY,
        });
    }
    shared.wake.notify_all();
}

fn service_one_due_debt(
    shared: &Shared,
    streams: &HashMap<StreamIdentity, PreparedStream>,
    debts: &mut VecDeque<DurabilityDebt>,
) {
    let Some(index) = debts
        .iter()
        .position(|debt| debt.retry_at <= Instant::now())
    else {
        return;
    };
    let mut debt = debts.remove(index).expect("located debt exists");
    if service_debt(shared, streams, &mut debt).is_ok() {
        clear_completed_directory_debt(shared, &mut debt);
        release_debt_bytes(shared, debt.charge);
    } else {
        report_failure(
            shared,
            Some(Arc::clone(&debt.identity)),
            PersistenceFailureKind::Sync,
        );
        debt.retry_at = Instant::now() + RETRY_DELAY;
        debts.push_back(debt);
    }
}

fn service_debt(
    shared: &Shared,
    streams: &HashMap<StreamIdentity, PreparedStream>,
    debt: &mut DurabilityDebt,
) -> io::Result<()> {
    streams
        .get(&debt.identity.stream)
        .filter(|stream| stream.generation == debt.identity.generation)
        .ok_or_else(|| io::Error::other("stream released before debt"))
        .and_then(|stream| shared.backend.sync_data(&stream.journal))
        .and_then(|()| publish_checkpoint(shared, debt))
        .and_then(|()| sync_stream_directories(shared, debt))
}

fn flush_release_debts(
    shared: &Shared,
    streams: &HashMap<StreamIdentity, PreparedStream>,
    debts: &mut VecDeque<DurabilityDebt>,
    identities: &[Arc<super::identity::LeaseIdentity>],
) -> io::Result<()> {
    let mut index = 0;
    while index < debts.len() {
        let matches = identities
            .iter()
            .any(|identity| Arc::ptr_eq(identity, &debts[index].identity));
        if !matches {
            index += 1;
            continue;
        }
        let mut debt = debts.remove(index).expect("release debt exists");
        if let Err(error) = service_debt(shared, streams, &mut debt) {
            debts.insert(index, debt);
            return Err(error);
        }
        clear_completed_directory_debt(shared, &mut debt);
        release_debt_bytes(shared, debt.charge);
    }
    Ok(())
}

fn service_one_due_touch(
    shared: &Shared,
    streams: &mut HashMap<StreamIdentity, PreparedStream>,
    touches: &mut VecDeque<TouchDebt>,
) {
    let Some(index) = touches
        .iter()
        .position(|debt| debt.retry_at <= Instant::now())
    else {
        return;
    };
    let mut debt = touches.remove(index).expect("located touch debt exists");
    let path = {
        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .streams
            .get(&debt.identity.stream)
            .filter(|stream| stream.generation == debt.identity.generation)
            .map(|stream| stream.journal_path.with_file_name("meta.json"))
    };
    if debt.prerequisite_written == Some(false) {
        return;
    }
    let result = path
        .ok_or_else(|| io::Error::other("stale touch debt"))
        .and_then(|path| {
            let stream = streams
                .get_mut(&debt.identity.stream)
                .filter(|stream| stream.generation == debt.identity.generation)
                .ok_or_else(|| io::Error::other("touch stream is not prepared"))?;
            if debt.prerequisite_written != Some(true) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "touch prerequisite is not written",
                ));
            }
            let meta = stream
                .session_meta
                .as_mut()
                .ok_or_else(|| io::Error::other("touch has no manifest authority"))?;
            let replacement = crate::SessionMeta {
                created_at: meta.created_at,
                last_touched: meta.last_touched.max(debt.last_touched),
            };
            write_manifest(shared, &path, &replacement)?;
            *meta = replacement;
            Ok(())
        });
    if let Err(error) = result {
        if error.kind() != io::ErrorKind::WouldBlock {
            report_failure(
                shared,
                Some(Arc::clone(&debt.identity)),
                PersistenceFailureKind::Sync,
            );
        }
        debt.retry_at = Instant::now() + RETRY_DELAY;
        touches.push_back(debt);
    }
}

/// Fixed retained charge reserved for each possible coalesced touch debt.
pub(crate) const fn touch_debt_charge() -> usize {
    std::mem::size_of::<TouchDebt>()
}

fn publish_checkpoint(shared: &Shared, debt: &DurabilityDebt) -> io::Result<()> {
    let Some(checkpoint) = debt.checkpoint.as_deref() else {
        return Ok(());
    };
    let checkpoint_path = {
        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .streams
            .get(&debt.identity.stream)
            .filter(|stream| stream.generation == debt.identity.generation)
            .map(|stream| stream.journal_path.with_file_name("meta.json"))
            .ok_or_else(|| io::Error::other("stale checkpoint debt"))?
    };
    let temporary = checkpoint_path.with_extension("json.tmp");
    let result = shared
        .backend
        .create_temporary_file(&temporary)
        .and_then(|mut file| {
            shared.backend.write_all(&mut file, checkpoint)?;
            shared.backend.sync_all(&file)
        })
        .and_then(|()| shared.backend.rename(&temporary, &checkpoint_path));
    if result.is_err() {
        let _ = shared.backend.remove_file(&temporary);
    }
    result
}

fn sync_stream_directories(shared: &Shared, debt: &DurabilityDebt) -> io::Result<()> {
    for target in debt.directory_targets.iter() {
        let directory = shared.backend.open_directory(target)?;
        shared.backend.sync_all(&directory)?;
    }
    Ok(())
}

fn debt_directory_targets(shared: &Shared, job: &FrameJob) -> Arc<[std::path::PathBuf]> {
    let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let Some(registered) = state
        .streams
        .get(job.stream())
        .filter(|stream| stream.generation == job.generation())
    else {
        return Arc::from([]);
    };
    if !registered.pending_directory_targets.is_empty() {
        return Arc::clone(&registered.pending_directory_targets);
    }
    if job.checkpoint.is_some()
        && let Some(parent) = registered.journal_path.parent()
    {
        return Arc::from([parent.to_path_buf()]);
    }
    Arc::from([])
}

fn clear_completed_directory_debt(shared: &Shared, debt: &mut DurabilityDebt) {
    if debt.directory_targets.is_empty() {
        return;
    }
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let Some(registered) = state.streams.get_mut(&debt.identity.stream) else {
        return;
    };
    if registered.generation != debt.identity.generation
        || registered.pending_directory_targets.is_empty()
        || !Arc::ptr_eq(
            &registered.pending_directory_targets,
            &debt.directory_targets,
        )
    {
        return;
    }
    let released = registered.directory_charge;
    registered.directory_charge = 0;
    registered.pending_directory_targets = Arc::from([]);
    let completed_targets = std::mem::replace(&mut debt.directory_targets, Arc::from([]));
    drop(completed_targets);
    registered.registry_charge = registered
        .registry_charge
        .checked_sub(released)
        .expect("directory registry charge releases exactly once");
    state.ledger.bytes = state
        .ledger
        .bytes
        .checked_sub(released)
        .expect("directory ledger charge releases exactly once");
}

/// Conservative owner-side fixed/key/path charge for one queued frame and debt.
pub(crate) fn worker_internal_charge(path_capacity: usize, directory_target_count: usize) -> usize {
    std::mem::size_of::<FrameJob>()
        .saturating_add(std::mem::size_of::<DurabilityDebt>())
        .saturating_add(2 * std::mem::size_of::<std::path::PathBuf>())
        .saturating_add(path_capacity.saturating_mul(2))
        .saturating_add(directory_target_count * std::mem::size_of::<usize>())
}

/// Persistent worker map, handle, and resumable-creation overhead per stream.
pub(crate) fn worker_persistent_stream_charge() -> usize {
    std::mem::size_of::<PreparedStream>()
        .saturating_add(std::mem::size_of::<NewAgentCreation>())
        .saturating_add(4 * std::mem::size_of::<File>())
}

fn minimum_deadline(
    head: Option<&FrameJob>,
    debts: &VecDeque<DurabilityDebt>,
    touches: &VecDeque<TouchDebt>,
) -> Option<Instant> {
    head.and_then(|job| job.retry_at)
        .into_iter()
        .chain(debts.iter().map(|debt| debt.retry_at))
        .chain(touches.iter().map(|debt| debt.retry_at))
        .min()
}

/// Writes one journal frame and restores the starting EOF after any ordinary
/// write error. `report_write_failure` runs after that write error and before
/// rollback performs its first filesystem operation.
pub(super) fn append_frame(
    backend: &dyn PersistenceBackend,
    file: &mut File,
    payload: &[u8],
    report_write_failure: impl FnOnce(),
) -> Result<u64, FrameAppendError> {
    let start = backend.seek_end(file).map_err(|_| FrameAppendError::Open)?;
    let length = u64::try_from(payload.len()).expect("usize fits u64");
    let result = backend
        .write_all(file, &length.to_le_bytes())
        .and_then(|()| backend.write_all(file, payload));
    if result
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::ConnectionAborted)
    {
        return Err(FrameAppendError::WorkerExit);
    }
    if result.is_err() {
        report_write_failure();
        return Err(match restore_eof(backend, file, start) {
            Ok(()) => FrameAppendError::RolledBack,
            Err(()) => FrameAppendError::RollbackFailed,
        });
    }
    Ok(start.saturating_add(8).saturating_add(length))
}

fn restore_eof(backend: &dyn PersistenceBackend, file: &mut File, expected: u64) -> Result<(), ()> {
    let current = backend.seek_end(file).map_err(|_| ())?;
    if current != expected {
        backend.truncate(file, expected).map_err(|_| ())?;
        let restored = backend.seek_end(file).map_err(|_| ())?;
        if restored != expected {
            return Err(());
        }
    }
    Ok(())
}

fn set_lifecycle(
    shared: &Shared,
    stream: &StreamIdentity,
    generation: PersistenceGeneration,
    lifecycle: StreamLifecycle,
) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(registered) = state.streams.get_mut(stream)
        && registered.generation == generation
    {
        registered.lifecycle = lifecycle;
    }
}

fn release_job(shared: &Shared, job: &FrameJob) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.ledger.frames = state
        .ledger
        .frames
        .checked_sub(1)
        .expect("terminal frame releases count exactly once");
    state.ledger.bytes = state
        .ledger
        .bytes
        .checked_sub(job.reserved_bytes)
        .expect("terminal frame releases bytes exactly once");
    super::owner::report_capacity_recovered(shared, &mut state);
    shared.wake.notify_all();
}

fn release_debt_bytes(shared: &Shared, bytes: usize) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.ledger.bytes = state
        .ledger
        .bytes
        .checked_sub(bytes)
        .expect("debt releases transferred bytes exactly once");
    super::owner::report_capacity_recovered(shared, &mut state);
    shared.wake.notify_all();
}

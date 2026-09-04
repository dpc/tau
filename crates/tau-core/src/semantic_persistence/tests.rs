//! Deterministic production-backend persistence failure oracles.

use std::fs::{File, Permissions};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use super::backend::{ExistingPathKind, FilesystemBackend, PersistenceBackend};
use super::worker::StreamLifecycle;
use super::{
    DurabilityBarrierOutcome, PersistenceCapacity, PersistenceFailureKind, RetentionCharge,
    SemanticPersistenceOwner, StagedFrame,
};
use crate::{AgentStore, SessionPreparationMode, SessionPreparationStatus, SessionStore};

/// One-shot exact-prefix write fault over the production filesystem backend.
struct WriteFaultBackend {
    fail_next_write: AtomicBool,
    fail_next_truncate: AtomicBool,
    fail_writes: AtomicBool,
    fail_writes_with_enospc: AtomicBool,
    hold_writes: AtomicBool,
    write_held: Mutex<bool>,
    write_hold_wake: Condvar,
    /// Number of writes that entered the deterministic blocking boundary.
    write_hold_entries: AtomicUsize,
    exit_next_write: AtomicBool,
    fail_next_sync_data: AtomicBool,
    fail_next_sync_all: AtomicBool,
    fail_next_rename: AtomicBool,
    lock_publication: LockPublicationFaults,
    fail_next_lock: AtomicBool,
    journal_writes: Mutex<Vec<String>>,
    renames: Mutex<usize>,
    rename_wake: Condvar,
    seek_call: AtomicUsize,
    seek_requires_observation: Mutex<Option<(usize, Arc<AtomicBool>)>>,
    write_call: AtomicUsize,
    short_write: Mutex<Option<(usize, usize)>>,
    fail_next_directory_create: AtomicBool,
    create_file_call: AtomicUsize,
    fail_create_file_call: AtomicUsize,
    failed_sync_path: Mutex<Option<String>>,
}

/// Fault controls for the pending-to-canonical lock publication protocol.
struct LockPublicationFaults {
    fail_next_remove_file: AtomicBool,
    hold_lock_publication: AtomicBool,
    lock_publication_held: Mutex<bool>,
    lock_publication_wake: Condvar,
    hold_before_pending_lock_create: AtomicBool,
    pending_lock_create_held: Mutex<bool>,
    pending_lock_create_wake: Condvar,
}

/// Projection whose selected retirement blocks until the test releases it.
struct HeldProjection {
    /// Encoded projection body.
    bytes: Vec<u8>,
    /// Whether this instance blocks while being dropped.
    hold_drop: bool,
    /// Drop-entered and release flags shared with the test.
    drop_state: Arc<(Mutex<(bool, bool)>, Condvar)>,
}

impl Drop for HeldProjection {
    fn drop(&mut self) {
        if !self.hold_drop {
            return;
        }
        let (state, wake) = &*self.drop_state;
        let mut state = state.lock().expect("projection drop state");
        state.0 = true;
        wake.notify_all();
        while !state.1 {
            state = wake.wait(state).expect("projection drop wait");
        }
        let _ = self.bytes.len();
    }
}

impl WriteFaultBackend {
    fn new() -> Self {
        Self {
            fail_next_write: AtomicBool::new(false),
            fail_next_truncate: AtomicBool::new(false),
            fail_writes: AtomicBool::new(false),
            fail_writes_with_enospc: AtomicBool::new(false),
            hold_writes: AtomicBool::new(false),
            write_held: Mutex::new(false),
            write_hold_wake: Condvar::new(),
            write_hold_entries: AtomicUsize::new(0),
            exit_next_write: AtomicBool::new(false),
            fail_next_sync_data: AtomicBool::new(false),
            fail_next_sync_all: AtomicBool::new(false),
            fail_next_rename: AtomicBool::new(false),
            lock_publication: LockPublicationFaults {
                fail_next_remove_file: AtomicBool::new(false),
                hold_lock_publication: AtomicBool::new(false),
                lock_publication_held: Mutex::new(false),
                lock_publication_wake: Condvar::new(),
                hold_before_pending_lock_create: AtomicBool::new(false),
                pending_lock_create_held: Mutex::new(false),
                pending_lock_create_wake: Condvar::new(),
            },
            fail_next_lock: AtomicBool::new(false),
            journal_writes: Mutex::new(Vec::new()),
            renames: Mutex::new(0),
            rename_wake: Condvar::new(),
            seek_call: AtomicUsize::new(0),
            seek_requires_observation: Mutex::new(None),
            write_call: AtomicUsize::new(0),
            short_write: Mutex::new(None),
            fail_next_directory_create: AtomicBool::new(false),
            create_file_call: AtomicUsize::new(0),
            fail_create_file_call: AtomicUsize::new(0),
            failed_sync_path: Mutex::new(None),
        }
    }

    fn rename_count(&self) -> usize {
        *self.renames.lock().expect("rename count")
    }

    fn wait_for_rename_after(&self, previous: usize) {
        let renames = self.renames.lock().expect("rename count");
        let (renames, _) = self
            .rename_wake
            .wait_timeout_while(renames, Duration::from_secs(2), |count| *count <= previous)
            .expect("rename wait");
        assert!(*renames > previous, "expected rename transition");
    }

    fn wait_until_lock_publication_held(&self) {
        let held = self
            .lock_publication
            .lock_publication_held
            .lock()
            .expect("publication held");
        let (held, _) = self
            .lock_publication
            .lock_publication_wake
            .wait_timeout_while(held, Duration::from_secs(2), |held| !*held)
            .expect("publication wait");
        assert!(*held, "worker did not reach held lock publication");
    }

    fn release_lock_publication(&self) {
        self.lock_publication
            .hold_lock_publication
            .store(false, Ordering::SeqCst);
        self.lock_publication.lock_publication_wake.notify_all();
    }

    fn wait_until_pending_lock_create_held(&self) {
        let held = self
            .lock_publication
            .pending_lock_create_held
            .lock()
            .expect("pending create held");
        let (held, _) = self
            .lock_publication
            .pending_lock_create_wake
            .wait_timeout_while(held, Duration::from_secs(2), |held| !*held)
            .expect("pending create wait");
        assert!(*held, "worker did not reach pending lock creation cut");
    }

    fn release_pending_lock_create(&self) {
        self.lock_publication
            .hold_before_pending_lock_create
            .store(false, Ordering::SeqCst);
        self.lock_publication.pending_lock_create_wake.notify_all();
    }

    fn inject_short_write(&self, call: usize, offset: usize) {
        self.write_call.store(0, Ordering::SeqCst);
        *self.short_write.lock().expect("short-write fault") = Some((call, offset));
    }

    fn require_observation_before_seek(&self, call: usize, observed: Arc<AtomicBool>) {
        self.seek_call.store(0, Ordering::SeqCst);
        *self
            .seek_requires_observation
            .lock()
            .expect("seek observation") = Some((call, observed));
    }

    fn wait_until_write_held(&self) {
        let held = self.write_held.lock().expect("write hold");
        let (held, _) = self
            .write_hold_wake
            .wait_timeout_while(held, Duration::from_secs(2), |held| !*held)
            .expect("write hold wait");
        assert!(*held, "worker did not reach held healthy write");
    }

    fn release_writes(&self) {
        self.hold_writes.store(false, Ordering::SeqCst);
        self.write_hold_wake.notify_all();
    }
}

impl PersistenceBackend for WriteFaultBackend {
    fn create_owner_directories(&self, path: &Path) -> io::Result<()> {
        FilesystemBackend.create_owner_directories(path)
    }
    fn create_owner_directory(&self, path: &Path) -> io::Result<()> {
        if self
            .fail_next_directory_create
            .swap(false, Ordering::SeqCst)
        {
            return Err(io::Error::other("injected directory creation failure"));
        }
        FilesystemBackend.create_owner_directory(path)
    }
    fn existing_path_kind(&self, path: &Path) -> io::Result<ExistingPathKind> {
        FilesystemBackend.existing_path_kind(path)
    }
    fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()> {
        FilesystemBackend.set_permissions(path, permissions)
    }
    fn create_new_file(&self, path: &Path) -> io::Result<File> {
        if path.file_name().is_some_and(|name| name == "lock.pending")
            && self
                .lock_publication
                .hold_before_pending_lock_create
                .load(Ordering::SeqCst)
        {
            let mut held = self
                .lock_publication
                .pending_lock_create_held
                .lock()
                .expect("pending create held");
            *held = true;
            self.lock_publication.pending_lock_create_wake.notify_all();
            while self
                .lock_publication
                .hold_before_pending_lock_create
                .load(Ordering::SeqCst)
            {
                held = self
                    .lock_publication
                    .pending_lock_create_wake
                    .wait(held)
                    .expect("pending create wait");
            }
            *held = false;
        }
        let call = self.create_file_call.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_create_file_call.load(Ordering::SeqCst) == call {
            self.fail_create_file_call.store(0, Ordering::SeqCst);
            return Err(io::Error::other("injected file creation failure"));
        }
        FilesystemBackend.create_new_file(path)
    }
    fn open_existing_file(&self, path: &Path) -> io::Result<File> {
        FilesystemBackend.open_existing_file(path)
    }
    fn open_existing_regular_file_read_no_follow(&self, path: &Path) -> io::Result<File> {
        FilesystemBackend.open_existing_regular_file_read_no_follow(path)
    }
    fn open_existing_regular_file_write_no_follow(&self, path: &Path) -> io::Result<File> {
        FilesystemBackend.open_existing_regular_file_write_no_follow(path)
    }
    fn create_temporary_file(&self, path: &Path) -> io::Result<File> {
        FilesystemBackend.create_temporary_file(path)
    }
    fn try_lock(&self, file: &File) -> io::Result<()> {
        if self.fail_next_lock.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected lock failure"));
        }
        FilesystemBackend.try_lock(file)
    }
    fn seek_end(&self, file: &mut File) -> io::Result<u64> {
        let call = self.seek_call.fetch_add(1, Ordering::SeqCst) + 1;
        let mut required = self
            .seek_requires_observation
            .lock()
            .expect("seek observation");
        if required.as_ref().is_some_and(|(target, _)| *target == call) {
            let (_, observed) = required.take().expect("matching seek observation");
            assert!(
                observed.load(Ordering::SeqCst),
                "write failure must be observed before rollback starts"
            );
        }
        drop(required);
        FilesystemBackend.seek_end(file)
    }
    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        let call = self.write_call.fetch_add(1, Ordering::SeqCst) + 1;
        let short_offset = {
            let mut fault = self.short_write.lock().expect("short-write fault");
            if fault.is_some_and(|(target, _)| target == call) {
                fault.take().map(|(_, offset)| offset)
            } else {
                None
            }
        };
        if let Some(offset) = short_offset {
            file.write_all(&bytes[..offset.min(bytes.len())])?;
            return Err(io::Error::other("injected exact-offset write failure"));
        }
        if self.hold_writes.load(Ordering::SeqCst) {
            self.write_hold_entries.fetch_add(1, Ordering::SeqCst);
            let mut held = self.write_held.lock().expect("write hold");
            *held = true;
            self.write_hold_wake.notify_all();
            while self.hold_writes.load(Ordering::SeqCst) {
                held = self.write_hold_wake.wait(held).expect("write hold wait");
            }
            *held = false;
        }
        #[cfg(target_os = "linux")]
        if let Ok(path) = std::fs::read_link(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(file)
        )) && path.file_name().is_some_and(|name| name == "events.cbor")
        {
            self.journal_writes
                .lock()
                .expect("write trace")
                .push(path.display().to_string());
        }
        if self.exit_next_write.swap(false, Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "injected worker exit",
            ));
        }
        if self.fail_writes.load(Ordering::SeqCst)
            || self.fail_next_write.swap(false, Ordering::SeqCst)
        {
            let prefix = bytes.len().min(3);
            file.write_all(&bytes[..prefix])?;
            return Err(io::Error::other("injected exact-prefix write failure"));
        }
        if self.fail_writes_with_enospc.load(Ordering::SeqCst) {
            let prefix = bytes.len().min(3);
            file.write_all(&bytes[..prefix])?;
            return Err(io::Error::from_raw_os_error(28));
        }
        FilesystemBackend.write_all(file, bytes)
    }
    fn truncate(&self, file: &File, offset: u64) -> io::Result<()> {
        if self.fail_next_truncate.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected rollback failure"));
        }
        FilesystemBackend.truncate(file, offset)
    }
    fn sync_data(&self, file: &File) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Ok(path) = std::fs::read_link(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(file)
        )) && self
            .failed_sync_path
            .lock()
            .expect("failed sync path")
            .as_ref()
            .is_some_and(|needle| path.to_string_lossy().contains(needle))
        {
            return Err(io::Error::other("injected persistent data sync failure"));
        }
        if self.fail_next_sync_data.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected data sync failure"));
        }
        FilesystemBackend.sync_data(file)
    }
    fn journal_position(
        &self,
        file: &File,
        end_offset: u64,
    ) -> io::Result<crate::agent_checkpoint::CommittedJournalPosition> {
        FilesystemBackend.journal_position(file, end_offset)
    }
    fn sync_all(&self, file: &File) -> io::Result<()> {
        if self.fail_next_sync_all.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected full sync failure"));
        }
        FilesystemBackend.sync_all(file)
    }
    fn rename(&self, source: &Path, destination: &Path) -> io::Result<()> {
        if self.fail_next_rename.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected rename failure"));
        }
        FilesystemBackend.rename(source, destination)?;
        *self.renames.lock().expect("rename count") += 1;
        self.rename_wake.notify_all();
        Ok(())
    }
    fn publish_no_replace(&self, source: &Path, destination: &Path) -> io::Result<()> {
        if source
            .file_name()
            .is_some_and(|name| name == "lock.pending")
            && self
                .lock_publication
                .hold_lock_publication
                .load(Ordering::SeqCst)
        {
            let mut held = self
                .lock_publication
                .lock_publication_held
                .lock()
                .expect("publication held");
            *held = true;
            self.lock_publication.lock_publication_wake.notify_all();
            while self
                .lock_publication
                .hold_lock_publication
                .load(Ordering::SeqCst)
            {
                held = self
                    .lock_publication
                    .lock_publication_wake
                    .wait(held)
                    .expect("publication wait");
            }
            *held = false;
        }
        FilesystemBackend.publish_no_replace(source, destination)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        if self
            .lock_publication
            .fail_next_remove_file
            .swap(false, Ordering::SeqCst)
        {
            return Err(io::Error::other("injected pending lock cleanup failure"));
        }
        FilesystemBackend.remove_file(path)
    }
    fn open_directory(&self, path: &Path) -> io::Result<File> {
        FilesystemBackend.open_directory(path)
    }
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        FilesystemBackend.read_file(path)
    }
    fn read_open_file(&self, file: &File) -> io::Result<Vec<u8>> {
        FilesystemBackend.read_open_file(file)
    }
}

/// A queued lifecycle release must discard a due lossy session touch before
/// entering its filesystem write, so shutdown cannot expire behind work that
/// the release itself makes obsolete.
#[test]
fn release_preempts_due_lossy_touch_before_filesystem_io() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(7),
        )
        .expect("append");
    assert!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(2))
            == DurabilityBarrierOutcome::Durable,
        "authoritative append and its durability debt drained"
    );
    owner.arm_derived_work_pause_for_test();
    store
        .record_session_activity("managed-session")
        .expect("queue lossy touch");
    assert!(
        owner.wait_for_derived_work_pause_for_test(Duration::from_secs(2)),
        "worker reached due derived work"
    );

    backend.hold_writes.store(true, Ordering::SeqCst);
    let leases = store.managed_persistence_leases("managed-session");
    let release_owner = Arc::clone(&owner);
    let release = thread::spawn(move || release_owner.release(&leases, Duration::from_secs(2)));
    assert!(
        owner.wait_for_release_command_for_test(Duration::from_secs(2)),
        "release command reached the worker queue"
    );
    owner.release_derived_work_pause_for_test();
    let result = release.join().expect("release thread");
    backend.release_writes();

    result.expect("queued release preempts obsolete lossy touch");
    assert_eq!(
        backend.write_hold_entries.load(Ordering::SeqCst),
        0,
        "release must not enter the obsolete touch write"
    );
}

/// Authoritative frames write in one FIFO across distinct prepared streams.
#[test]
fn authoritative_frames_preserve_cross_stream_fifo() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    for session in ["first-session", "second-session"] {
        store
            .prepare_session(session, SessionPreparationMode::New)
            .expect("prepare session");
    }
    for session in ["first-session", "second-session"] {
        store
            .append_session_event_at(
                session,
                None,
                loaded_event(session),
                tau_proto::UnixMicros::new(7),
            )
            .expect("append");
    }
    let mut leases = store.managed_persistence_leases("first-session");
    leases.extend(store.managed_persistence_leases("second-session"));
    owner
        .release(&leases, Duration::from_secs(2))
        .expect("drain both streams");
    let writes = backend.journal_writes.lock().expect("write trace");
    let first = writes
        .iter()
        .position(|path| path.contains("first-session"))
        .expect("first stream write");
    let second = writes
        .iter()
        .position(|path| path.contains("second-session"))
        .expect("second stream write");
    assert!(first < second);
}

/// Unexpected sole-worker exit invalidates every generation immediately.
#[test]
fn worker_exit_makes_every_lease_unavailable() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    owner.set_operational_wake(Arc::new(move || {
        let _ = wake_tx.try_send(());
    }));
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    backend.exit_next_write.store(true, Ordering::SeqCst);
    store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(7),
        )
        .expect("frame accepted before injected worker exit");
    wake_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("worker exit wakes operational observer");
    assert!(
        owner
            .drain_operational_status()
            .failures
            .iter()
            .any(|failure| failure.kind() == PersistenceFailureKind::WorkerExit)
    );
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(2)),
        DurabilityBarrierOutcome::UnavailableOrFailed,
        "worker exit must preserve the unavailable durability classification"
    );
    let error = store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(8),
        )
        .expect_err("dead owner rejects every generation");
    assert!(error.to_string().contains("unavailable"));
}

/// Queued plus in-flight frames share one exact hard admission boundary.
#[test]
fn in_flight_frame_consumes_the_only_capacity_permit() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity {
                max_frames: 1,
                max_bytes: 256 * 1024 * 1024,
                max_streams: 8,
            },
            backend.clone(),
        )
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    backend.fail_writes.store(true, Ordering::SeqCst);
    store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(7),
        )
        .expect("first frame accepted");
    let error = store
        .append_session_event_at(
            "managed-session",
            None,
            tau_proto::Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                session_id: tau_proto::SessionId::parse("managed-session").expect("session"),
                agent_id: tau_proto::AgentId::parse("managed-agent").expect("agent"),
            }),
            tau_proto::UnixMicros::new(8),
        )
        .expect_err("in-flight frame owns the sole permit");
    assert!(error.to_string().contains("full"));
    let pressure = owner
        .drain_operational_status()
        .capacity_full
        .expect("frame pressure");
    assert_eq!(pressure.limit, super::PersistenceCapacityLimit::Frames);
    assert_eq!(pressure.usage.frames, 1);
    assert_eq!(
        store
            .session_events("managed-session")
            .expect("live projection")
            .len(),
        1,
        "rejected append did not mutate projection"
    );
    backend.fail_writes.store(false, Ordering::SeqCst);
    let leases = store.managed_persistence_leases("managed-session");
    owner
        .release(&leases, Duration::from_secs(2))
        .expect("retained head retries and releases");
}

/// Stream registration reports only the exact stream-permit boundary and
/// observes rollback of the partially prepared session as recovered capacity.
#[test]
fn stream_capacity_pressure_reports_exact_registration_boundary() {
    let root = tempfile::tempdir().expect("temporary root");
    let owner = Arc::new(
        SemanticPersistenceOwner::new(PersistenceCapacity {
            max_frames: 8,
            max_bytes: 256 * 1024 * 1024,
            max_streams: 1,
        })
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    let error = store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect_err("ordinary plus restore streams exceed one permit");
    assert!(error.to_string().contains("full"));
    let status = owner.drain_operational_status();
    let pressure = status.capacity_full.expect("stream pressure");
    assert_eq!(pressure.limit, super::PersistenceCapacityLimit::Streams);
    assert_eq!(pressure.usage.streams, 1);
    assert_eq!(status.recovered.expect("registration rollback").streams, 0);
}

/// A waiter parked before an unprovable rollback must observe its failure only
/// after the matching generation is poisoned, so no later admission can pass.
#[test]
fn rollback_failure_poison_rejects_later_admission() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    backend.hold_writes.store(true, Ordering::SeqCst);
    backend.fail_next_write.store(true, Ordering::SeqCst);
    backend.fail_next_truncate.store(true, Ordering::SeqCst);
    store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(7),
        )
        .expect("first live fact was accepted before worker poison");
    backend.wait_until_write_held();
    let (ready_send, ready_receive) = mpsc::sync_channel(0);
    let waiter_owner = Arc::clone(&owner);
    let waiter = thread::spawn(move || {
        waiter_owner.wait_for_failure_after_ready_for_test(
            PersistenceFailureKind::Rollback,
            Duration::from_secs(2),
            ready_send,
        )
    });
    ready_receive
        .recv_timeout(Duration::from_secs(2))
        .expect("failure waiter parked before rollback");
    backend.release_writes();
    assert!(
        waiter.join().expect("failure waiter"),
        "parked waiter observed exact rollback failure"
    );
    assert_eq!(
        owner.rollback_failure_lifecycle_at_publication_for_test(),
        Some(StreamLifecycle::Poisoned),
        "failure insertion and poison transition share one state cut"
    );
    let error = store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(8),
        )
        .expect_err("poison rejects later admission before projection mutation");
    assert!(error.to_string().contains("poisoned"));
}

fn loaded_event(session_id: &str) -> tau_proto::Event {
    tau_proto::Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
        agent_initialization_id: tau_proto::AgentInitializationId::parse("managed-init")
            .expect("test initialization id"),
        session_id: tau_proto::SessionId::parse(session_id).expect("test session id"),
        agent_id: tau_proto::AgentId::parse("managed-agent").expect("test agent id"),
        ephemeral: false,
    })
}

fn started_event(agent_id: &tau_proto::AgentId) -> tau_proto::Event {
    tau_proto::Event::AgentStarted(tau_proto::AgentStarted {
        creator: Some(tau_proto::AgentCreator::default()),
        agent_id: agent_id.clone(),
        parent_agent: None,
        role: "engineer".to_owned(),
        display_name: None,
        metadata: Vec::new(),
        ephemeral: false,
    })
}

fn display_name_event(agent_id: &tau_proto::AgentId, display_name: &str) -> tau_proto::Event {
    tau_proto::Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
        agent_id: agent_id.clone(),
        display_name: display_name.to_owned(),
    })
}

/// A new-agent reservation rejects an existing live path before any accepted
/// creation work can truncate or delete canonical bytes.
#[test]
fn new_agent_collision_preserves_existing_bytes() {
    let root = tempfile::tempdir().expect("temporary root");
    let agents = root.path().join("agents");
    std::fs::create_dir_all(agents.join("collision-agent")).expect("collision directory");
    let journal = agents.join("collision-agent/events.cbor");
    let old_bytes = b"existing-canonical-bytes";
    std::fs::write(&journal, old_bytes).expect("seed canonical bytes");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = AgentStore::open_managed(&agents, owner).expect("store");
    let agent_id = tau_proto::AgentId::parse("collision-agent").expect("agent id");
    let error = store
        .reserve_new_agent(agent_id.as_str())
        .expect_err("live disk path reserves the id");
    assert!(matches!(
        error,
        crate::AgentStoreError::PersistenceConflict { .. }
    ));
    assert_eq!(
        std::fs::read(journal).expect("old journal"),
        old_bytes,
        "reservation rejection must preserve existing canonical bytes"
    );
}

/// Every fallible new-agent creation phase resumes its own artifacts without
/// cleanup, duplicate creation, or losing the accepted projection.
#[test]
fn new_agent_creation_retries_each_phased_cut() {
    enum Fault {
        Directory,
        LockFile,
        Lock,
        Journal,
    }
    for fault in [
        Fault::Directory,
        Fault::LockFile,
        Fault::Lock,
        Fault::Journal,
    ] {
        let root = tempfile::tempdir().expect("temporary root");
        let backend = Arc::new(WriteFaultBackend::new());
        let owner = Arc::new(
            SemanticPersistenceOwner::with_test_backend(
                PersistenceCapacity::default(),
                backend.clone(),
            )
            .expect("owner"),
        );
        let agents = root.path().join("agents");
        let mut store = AgentStore::open_managed(&agents, owner.clone()).expect("store");
        let agent_id = tau_proto::AgentId::parse("phased-agent").expect("agent id");
        store.reserve_new_agent(agent_id.as_str()).expect("reserve");
        backend.create_file_call.store(0, Ordering::SeqCst);
        let expected = match fault {
            Fault::Directory => {
                backend
                    .fail_next_directory_create
                    .store(true, Ordering::SeqCst);
                PersistenceFailureKind::Open
            }
            Fault::LockFile => {
                backend.fail_create_file_call.store(1, Ordering::SeqCst);
                PersistenceFailureKind::Open
            }
            Fault::Lock => {
                backend.fail_next_lock.store(true, Ordering::SeqCst);
                PersistenceFailureKind::Lock
            }
            Fault::Journal => {
                backend.fail_create_file_call.store(2, Ordering::SeqCst);
                PersistenceFailureKind::Open
            }
        };
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                crate::AgentEventParent::InheritHead,
                started_event(&agent_id),
                tau_proto::UnixMicros::new(7),
            )
            .expect("creation projection accepted");
        assert!(store.agent_has_committed_identity(&agent_id));
        assert!(owner.wait_for_failure_for_test(expected, Duration::from_secs(2)));
        owner
            .release(&store.managed_persistence_leases(), Duration::from_secs(2))
            .expect("phase retry completes before release");
        assert!(agents.join(agent_id.as_str()).join("events.cbor").exists());
    }
}

/// Reserved-new admission accepts only its exact sequence-zero AgentStarted and
/// preserves the reservation after wrong-first rejection.
#[test]
fn reserved_new_agent_rejects_wrong_first_without_consuming_reservation() {
    let root = tempfile::tempdir().expect("temporary root");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store =
        AgentStore::open_managed(root.path().join("agents"), owner.clone()).expect("store");
    let agent_id = tau_proto::AgentId::parse("reserved-agent").expect("agent id");
    store.reserve_new_agent(agent_id.as_str()).expect("reserve");
    let error = store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            tau_proto::Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: agent_id.clone(),
                display_name: "wrong first".to_owned(),
            }),
            tau_proto::UnixMicros::new(7),
        )
        .expect_err("wrong first event rejects");
    assert!(error.to_string().contains("not prepared"));
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            started_event(&agent_id),
            tau_proto::UnixMicros::new(8),
        )
        .expect("matching first fact still consumes reservation");
}

/// Strict Resume rejects missing authority without reconstructing a manifest.
#[test]
fn resume_missing_manifest_and_journals_creates_nothing() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store root preparation");
    let error = store
        .prepare_session("missing-session", SessionPreparationMode::Resume)
        .expect_err("strict resume rejects missing stream");
    assert!(error.to_string().contains("no longer exists"));
    assert!(!sessions.join("missing-session").exists());
}

/// New preparation may claim pre-existing noncanonical session scaffolding
/// without truncating it; create-new canonical files still detect collisions.
#[test]
fn new_session_claims_existing_diagnostic_scaffolding() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let session = sessions.join("managed-session");
    std::fs::create_dir_all(session.join("logs")).expect("diagnostic scaffolding");
    std::fs::write(session.join("logs/relay.log"), b"preexisting diagnostic")
        .expect("diagnostic file");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("claim scaffolding");
    assert_eq!(
        std::fs::read(session.join("logs/relay.log")).expect("diagnostic file"),
        b"preexisting diagnostic"
    );
    assert!(session.join("meta.json").is_file());
    assert!(session.join("events.cbor").is_file());
    assert!(session.join("restore-events.cbor").is_file());
}

/// Exclusive creation fails closed on even noncanonical scaffolding and leaves
/// every existing byte untouched instead of claiming, repairing, or deleting
/// it.
#[test]
fn exclusive_session_creation_requires_complete_directory_absence() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let session = sessions.join("managed-session");
    std::fs::create_dir_all(session.join("logs")).expect("diagnostic scaffolding");
    let diagnostic = session.join("logs/relay.log");
    std::fs::write(&diagnostic, b"preexisting diagnostic").expect("diagnostic file");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store");

    let error = store
        .prepare_session("managed-session", SessionPreparationMode::Create)
        .expect_err("exclusive creation must reject any existing directory");

    assert!(error.to_string().contains("already exists"));
    assert_eq!(
        std::fs::read(&diagnostic).expect("preserved diagnostic"),
        b"preexisting diagnostic"
    );
    assert!(!session.join("lock").exists());
    assert!(!session.join("meta.json").exists());
    assert!(!session.join("events.cbor").exists());
    assert!(!session.join("restore-events.cbor").exists());
}

/// Create-or-resume atomically creates an absent session and strictly resumes
/// the exact canonical state after the first owner releases it.
#[test]
fn create_or_resume_selects_created_then_resumed_lifecycle() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let first_owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut first =
        SessionStore::open_managed(&sessions, first_owner.clone()).expect("first store");
    assert_eq!(
        first
            .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
            .expect("create absent session"),
        SessionPreparationStatus::Created
    );
    first_owner
        .release(
            &first.managed_persistence_leases("managed-session"),
            Duration::from_secs(2),
        )
        .expect("release first owner");

    let second_owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut second = SessionStore::open_managed(&sessions, second_owner).expect("second store");
    assert_eq!(
        second
            .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
            .expect("resume valid session"),
        SessionPreparationStatus::Resumed
    );
}

/// Competing create-or-resume owners share the atomic directory boundary: one
/// creates the session while the other fails rather than replacing or repairing
/// the winner's locked state.
#[test]
fn create_or_resume_race_has_one_owner_and_one_failure() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("sessions root");
    let start = Arc::new(Barrier::new(3));
    let finish = Arc::new(Barrier::new(3));
    let (tx, rx) = mpsc::channel();
    let mut threads = Vec::new();
    for _ in 0..2 {
        let sessions = sessions.clone();
        let start = start.clone();
        let finish = finish.clone();
        let tx = tx.clone();
        threads.push(thread::spawn(move || {
            let owner = Arc::new(
                SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"),
            );
            let mut store = SessionStore::open_managed(&sessions, owner).expect("store");
            start.wait();
            let result =
                store.prepare_session("managed-session", SessionPreparationMode::CreateOrResume);
            tx.send(result.map_err(|error| error.to_string()))
                .expect("send preparation result");
            finish.wait();
        }));
    }
    drop(tx);
    start.wait();
    let results = [
        rx.recv_timeout(Duration::from_secs(2))
            .expect("first result"),
        rx.recv_timeout(Duration::from_secs(2))
            .expect("second result"),
    ];
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Ok(SessionPreparationStatus::Created))
            .count(),
        1
    );
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    finish.wait();
    for thread in threads {
        thread.join().expect("join preparation owner");
    }
}

/// The mkdir winner locks a private pending inode before publishing the
/// canonical lock path, so a concurrent resume cannot steal the creator's lock
/// during first-session initialization.
#[test]
fn create_or_resume_creator_cannot_lose_lock_publication_race() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let backend = Arc::new(WriteFaultBackend::new());
    backend
        .lock_publication
        .hold_lock_publication
        .store(true, Ordering::SeqCst);
    let creator_owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("creator owner"),
    );
    let mut creator = SessionStore::open_managed(&sessions, creator_owner).expect("creator store");
    let (creator_tx, creator_rx) = mpsc::channel();
    let creator_thread = thread::spawn(move || {
        creator_tx
            .send(
                creator
                    .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
                    .map_err(|error| error.to_string()),
            )
            .expect("send creator result");
    });
    backend.wait_until_lock_publication_held();

    let session = sessions.join("managed-session");
    assert!(session.join("lock.pending").is_file());
    assert!(!session.join("lock").exists());
    let loser_owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut loser = SessionStore::open_managed(&sessions, loser_owner).expect("loser store");
    loser
        .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
        .expect_err("concurrent resume must not claim the unpublished lock");

    backend.release_lock_publication();
    assert_eq!(
        creator_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("creator result"),
        Ok(SessionPreparationStatus::Created)
    );
    creator_thread.join().expect("join creator");
    assert!(session.join("lock").is_file());
    assert!(!session.join("lock.pending").exists());
}

/// No-replace publication prevents a delayed create-or-resume winner from
/// overwriting the live canonical lock installed by an ordinary New
/// initializer.
#[test]
fn create_or_resume_cannot_replace_concurrent_new_mode_lock() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let backend = Arc::new(WriteFaultBackend::new());
    backend
        .lock_publication
        .hold_before_pending_lock_create
        .store(true, Ordering::SeqCst);
    let delayed_owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("delayed owner"),
    );
    let mut delayed = SessionStore::open_managed(&sessions, delayed_owner).expect("delayed store");
    let (delayed_tx, delayed_rx) = mpsc::channel();
    let delayed_thread = thread::spawn(move || {
        delayed_tx
            .send(
                delayed
                    .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
                    .map_err(|error| error.to_string()),
            )
            .expect("send delayed result");
    });
    backend.wait_until_pending_lock_create_held();

    let current_owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut current = SessionStore::open_managed(&sessions, current_owner).expect("current store");
    current
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("concurrent New preparation");

    backend.release_pending_lock_create();
    delayed_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("delayed result")
        .expect_err("delayed publisher must not replace canonical lock");
    delayed_thread.join().expect("join delayed owner");
    assert!(
        !sessions.join("managed-session/lock.pending").exists(),
        "failed no-replace publication must clean its private pending alias"
    );

    let third_owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut third = SessionStore::open_managed(&sessions, third_owner).expect("third store");
    third
        .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
        .expect_err("current owner's canonical lock must remain authoritative");
}

/// Failure to remove the private alias after canonical hard-link publication
/// cannot turn committed lock authority into a reported creation failure.
#[test]
fn create_or_resume_committed_lock_survives_pending_cleanup_failure() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let backend = Arc::new(WriteFaultBackend::new());
    backend
        .lock_publication
        .fail_next_remove_file
        .store(true, Ordering::SeqCst);
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(PersistenceCapacity::default(), backend)
            .expect("owner"),
    );
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store");
    assert_eq!(
        store
            .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
            .expect("committed canonical lock remains successful"),
        SessionPreparationStatus::Created
    );
    let session = sessions.join("managed-session");
    assert!(session.join("lock").is_file());
    assert!(session.join("lock.pending").is_file());

    let contender_owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut contender =
        SessionStore::open_managed(&sessions, contender_owner).expect("contender store");
    contender
        .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
        .expect_err("canonical committed lock must exclude another owner");
}

/// Create-or-resume treats partial state as occupied authority and preserves it
/// byte-for-byte instead of filling in missing canonical siblings.
#[test]
fn create_or_resume_rejects_partial_state_without_mutation() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let session = sessions.join("managed-session");
    std::fs::create_dir_all(&session).expect("partial session");
    let artifact = session.join("events.cbor");
    std::fs::write(&artifact, b"partial").expect("partial journal");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
        .expect_err("partial state must fail");
    assert_eq!(
        std::fs::read(&artifact).expect("partial journal"),
        b"partial"
    );
    assert!(!session.join("lock").exists());
    assert!(!session.join("meta.json").exists());
    assert!(!session.join("restore-events.cbor").exists());
}

/// Create-or-resume classifies a torn final frame header as partial state and
/// leaves the ordinary journal byte-for-byte unchanged.
#[test]
fn create_or_resume_rejects_torn_header_without_truncation() {
    assert_create_or_resume_preserves_torn_tail("events.cbor", &[1, 2, 3]);
}

/// Create-or-resume classifies a torn final frame payload as partial state and
/// leaves the restore journal byte-for-byte unchanged.
#[test]
fn create_or_resume_rejects_torn_payload_without_truncation() {
    let mut tail = 10_u64.to_le_bytes().to_vec();
    tail.extend_from_slice(&[0xa1, 0x61]);
    assert_create_or_resume_preserves_torn_tail("restore-events.cbor", &tail);
}

/// Seeds one valid session, appends an incomplete frame, and verifies strict
/// create-or-resume admission cannot invoke ordinary recovery truncation.
fn assert_create_or_resume_preserves_torn_tail(journal_name: &str, tail: &[u8]) {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let first_owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut first = SessionStore::open_managed(&sessions, first_owner.clone()).expect("store");
    first
        .prepare_session("managed-session", SessionPreparationMode::Create)
        .expect("seed session");
    first_owner
        .release(
            &first.managed_persistence_leases("managed-session"),
            Duration::from_secs(2),
        )
        .expect("release seed");
    let journal = sessions.join("managed-session").join(journal_name);
    let mut before = std::fs::read(&journal).expect("seed journal");
    before.extend_from_slice(tail);
    std::fs::write(&journal, &before).expect("write torn journal");

    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
        .expect_err("torn journal must fail");
    assert_eq!(
        std::fs::read(journal).expect("preserved torn journal"),
        before
    );
}

/// Create-or-resume never follows a symlink occupying the exact session path,
/// even when its target contains otherwise valid canonical state.
#[cfg(unix)]
#[test]
fn create_or_resume_rejects_symlinked_session_path() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let target_sessions = root.path().join("target-sessions");
    let target_owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut target =
        SessionStore::open_managed(&target_sessions, target_owner.clone()).expect("target store");
    target
        .prepare_session("managed-session", SessionPreparationMode::Create)
        .expect("create target session");
    target_owner
        .release(
            &target.managed_persistence_leases("managed-session"),
            Duration::from_secs(2),
        )
        .expect("release target");
    std::fs::create_dir_all(&sessions).expect("sessions root");
    symlink(
        target_sessions.join("managed-session"),
        sessions.join("managed-session"),
    )
    .expect("session symlink");

    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store");
    let error = store
        .prepare_session("managed-session", SessionPreparationMode::CreateOrResume)
        .expect_err("symlinked state must fail");
    assert!(
        error.to_string().contains("not a real directory"),
        "{error}"
    );
}

/// Create-or-resume opens every canonical child as an exact regular file, so no
/// child symlink can borrow another session's lock, manifest, or journals.
#[cfg(unix)]
#[test]
fn create_or_resume_rejects_every_symlinked_canonical_child() {
    for artifact in ["lock", "meta.json", "events.cbor", "restore-events.cbor"] {
        assert_create_or_resume_rejects_symlinked_canonical_child(artifact);
    }
}

/// Read-only manifests remain valid because both ordinary resume and strict
/// create-or-resume require only descriptor read access to `meta.json`.
#[cfg(unix)]
#[test]
fn resume_modes_accept_read_only_regular_manifest() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    for (session_id, mode) in [
        ("ordinary-resume", SessionPreparationMode::Resume),
        ("create-or-resume", SessionPreparationMode::CreateOrResume),
    ] {
        let seed_owner = Arc::new(
            SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("seed owner"),
        );
        let mut seed =
            SessionStore::open_managed(&sessions, seed_owner.clone()).expect("seed store");
        seed.prepare_session(session_id, SessionPreparationMode::Create)
            .expect("seed session");
        seed_owner
            .release(
                &seed.managed_persistence_leases(session_id),
                Duration::from_secs(2),
            )
            .expect("release seed");
        let manifest = sessions.join(session_id).join("meta.json");
        std::fs::set_permissions(&manifest, Permissions::from_mode(0o400))
            .expect("read-only manifest");

        let owner = Arc::new(
            SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("resume owner"),
        );
        let mut store = SessionStore::open_managed(&sessions, owner).expect("resume store");
        assert_eq!(
            store
                .prepare_session(session_id, mode)
                .expect("read-only manifest remains valid"),
            SessionPreparationStatus::Resumed
        );
    }
}

/// Unix no-follow admission uses nonblocking open only to classify special
/// files, then clears that flag before retaining regular descriptors.
#[cfg(unix)]
#[test]
fn no_follow_regular_open_clears_nonblocking_status() {
    use rustix_v1::fs::{OFlags, fcntl_getfl};

    let root = tempfile::tempdir().expect("temporary root");
    let path = root.path().join("canonical");
    std::fs::write(&path, b"canonical").expect("canonical file");
    for file in [
        FilesystemBackend
            .open_existing_regular_file_read_no_follow(&path)
            .expect("read-only no-follow open"),
        FilesystemBackend
            .open_existing_regular_file_write_no_follow(&path)
            .expect("read/write no-follow open"),
    ] {
        let flags = fcntl_getfl(file).expect("read descriptor status flags");
        assert!(!flags.contains(OFlags::NONBLOCK));
    }
}

/// Seeds two valid sessions and replaces one canonical child with a symlink to
/// prove strict admission rejects the exact child without changing either side.
#[cfg(unix)]
fn assert_create_or_resume_rejects_symlinked_canonical_child(artifact: &str) {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = SessionStore::open_managed(&sessions, owner.clone()).expect("store");
    for session_id in ["source", "target"] {
        store
            .prepare_session(session_id, SessionPreparationMode::Create)
            .expect("seed session");
    }
    owner
        .release(
            &[
                store.managed_persistence_leases("source"),
                store.managed_persistence_leases("target"),
            ]
            .concat(),
            Duration::from_secs(2),
        )
        .expect("release seed sessions");

    let source = sessions.join("source").join(artifact);
    let target = sessions.join("target").join(artifact);
    let target_before = std::fs::read(&target).expect("target canonical bytes");
    std::fs::remove_file(&source).expect("remove source canonical child");
    symlink(&target, &source).expect("install canonical child symlink");

    let resume_owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut resume = SessionStore::open_managed(&sessions, resume_owner).expect("resume store");
    resume
        .prepare_session("source", SessionPreparationMode::CreateOrResume)
        .expect_err("symlinked canonical child must fail");
    assert!(
        std::fs::symlink_metadata(&source)
            .expect("source symlink")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read(target).expect("preserved target canonical bytes"),
        target_before
    );
}

/// New preparation rejects partial canonical state before creating any sibling
/// artifacts and preserves the existing manifest byte-for-byte.
#[test]
fn new_session_partial_manifest_collision_is_non_destructive() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let session = sessions.join("managed-session");
    std::fs::create_dir_all(&session).expect("partial session directory");
    let manifest = session.join("meta.json");
    let old_bytes = b"{\"partial\":\"canonical\"}";
    std::fs::write(&manifest, old_bytes).expect("partial manifest");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store");
    let error = store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect_err("partial canonical collision rejects");
    assert!(error.to_string().contains("already exists"));
    assert_eq!(std::fs::read(manifest).expect("manifest bytes"), old_bytes);
    assert!(!session.join("lock").exists());
    assert!(!session.join("events.cbor").exists());
    assert!(!session.join("restore-events.cbor").exists());
}

/// Duplicate capabilities reject before closing any member of the release set.
#[test]
fn duplicate_release_set_is_atomic_and_leaves_lease_live() {
    let root = tempfile::tempdir().expect("temporary root");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    let leases = store.managed_persistence_leases("managed-session");
    let duplicate = [leases[0].clone(), leases[0].clone()];
    assert!(owner.release(&duplicate, Duration::from_secs(1)).is_err());
    store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(7),
        )
        .expect("failed group validation left the generation live");
    owner
        .release(&leases, Duration::from_secs(2))
        .expect("exact unique group releases");
}

/// Touch writeback uses cached prepared manifest authority and never
/// rediscovers a concurrently replaced file.
#[test]
fn touch_preserves_prepared_created_at_without_live_manifest_read() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    let path = sessions.join("managed-session/meta.json");
    let prepared: crate::SessionMeta =
        serde_json::from_slice(&std::fs::read(&path).expect("prepared manifest"))
            .expect("decode manifest");
    std::fs::write(
        &path,
        serde_json::to_vec(&crate::SessionMeta {
            created_at: 7,
            last_touched: 8,
        })
        .expect("replacement manifest"),
    )
    .expect("replace live file outside authority");
    let previous_renames = backend.rename_count();
    store
        .record_session_activity("managed-session")
        .expect("touch admission");
    backend.wait_for_rename_after(previous_renames);
    let current: crate::SessionMeta =
        serde_json::from_slice(&std::fs::read(&path).expect("manifest")).expect("decode manifest");
    assert!(current.last_touched > 8);
    assert_eq!(current.created_at, prepared.created_at);
}

/// Data-sync failure remains charged debt and release retries it to completion.
#[test]
fn data_sync_failure_is_diagnosed_and_retried_before_release() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    backend.fail_next_sync_data.store(true, Ordering::SeqCst);
    store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(7),
        )
        .expect("append accepted");
    assert!(owner.wait_for_failure_for_test(PersistenceFailureKind::Sync, Duration::from_secs(2),));
    owner
        .release(
            &store.managed_persistence_leases("managed-session"),
            Duration::from_secs(2),
        )
        .expect("release drains retried sync debt");
}

/// Root/ancestor directory debt keeps its failed deadline while later FIFO
/// traffic advances the same stream, then releases exactly once.
#[test]
fn directory_sync_debt_retries_under_continuous_frame_traffic() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let baseline = owner.ledger_for_test().1;
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    backend.fail_next_sync_all.store(true, Ordering::SeqCst);
    store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(7),
        )
        .expect("first append");
    store
        .append_session_event_at(
            "managed-session",
            None,
            tau_proto::Event::SessionAgentUnloaded(tau_proto::SessionAgentUnloaded {
                session_id: tau_proto::SessionId::parse("managed-session").expect("session"),
                agent_id: tau_proto::AgentId::parse("managed-agent").expect("agent"),
            }),
            tau_proto::UnixMicros::new(8),
        )
        .expect("later traffic remains accepted");
    assert!(owner.wait_for_failure_for_test(PersistenceFailureKind::Sync, Duration::from_secs(2),));
    owner
        .release(
            &store.managed_persistence_leases("managed-session"),
            Duration::from_secs(2),
        )
        .expect("directory debt retry drains");
    assert_eq!(owner.ledger_for_test(), (0, baseline, 0));
}

/// A barrier after two cross-stream frames cannot be satisfied by later debt
/// overtaking an earlier failed debt.
#[cfg(target_os = "linux")]
#[test]
fn durability_barrier_waits_for_every_prior_cross_stream_debt() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    for session in ["first-session", "second-session"] {
        store
            .prepare_session(session, SessionPreparationMode::New)
            .expect("prepare");
    }
    *backend.failed_sync_path.lock().expect("failed sync path") =
        Some("first-session/events.cbor".to_owned());
    for session in ["first-session", "second-session"] {
        store
            .append_session_event_at(
                session,
                None,
                loaded_event(session),
                tau_proto::UnixMicros::new(7),
            )
            .expect("append");
    }
    assert!(owner.wait_for_failure_for_test(PersistenceFailureKind::Sync, Duration::from_secs(2),));
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_millis(50)),
        DurabilityBarrierOutcome::DeadlineExpired,
        "later stream debt must not satisfy a barrier over earlier failed debt"
    );
    *backend.failed_sync_path.lock().expect("failed sync path") = None;
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(2)),
        DurabilityBarrierOutcome::Durable
    );
}

/// A checkpoint/touch rename failure preserves its deadline-owned debt and
/// eventually publishes the cached manifest replacement.
#[test]
fn touch_rename_failure_retries_cached_debt() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let sessions = root.path().join("sessions");
    let mut store = SessionStore::open_managed(&sessions, owner.clone()).expect("store");
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    backend.fail_next_rename.store(true, Ordering::SeqCst);
    store
        .record_session_activity("managed-session")
        .expect("touch accepted");
    assert!(owner.wait_for_failure_for_test(PersistenceFailureKind::Sync, Duration::from_secs(2),));
    owner
        .release(
            &store.managed_persistence_leases("managed-session"),
            Duration::from_secs(2),
        )
        .expect("release waits for touch retry");
}

/// Strict existing preparation reports a lock failure without admitting a
/// lease.
#[test]
fn strict_resume_lock_failure_does_not_prepare_streams() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    {
        let owner = Arc::new(
            SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("seed owner"),
        );
        let mut store = SessionStore::open_managed(&sessions, owner.clone()).expect("store");
        store
            .prepare_session("managed-session", SessionPreparationMode::New)
            .expect("prepare seed");
        owner
            .release(
                &store.managed_persistence_leases("managed-session"),
                Duration::from_secs(2),
            )
            .expect("seed release");
    }
    let backend = Arc::new(WriteFaultBackend::new());
    backend.fail_next_lock.store(true, Ordering::SeqCst);
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(PersistenceCapacity::default(), backend)
            .expect("owner"),
    );
    let mut store = SessionStore::open_managed(&sessions, owner).expect("store");
    let error = store
        .prepare_session("managed-session", SessionPreparationMode::Resume)
        .expect_err("lock failure rejects preparation");
    assert!(error.to_string().contains("lock failure"));
    assert!(
        store
            .managed_persistence_leases("managed-session")
            .is_empty()
    );
}

/// A second owner cannot acquire an already-prepared generation's strict lock.
#[test]
fn duplicate_owner_is_rejected_without_second_mutable_handle_set() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let first =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut first_store = SessionStore::open_managed(&sessions, first).expect("first store");
    first_store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("first preparation");

    let second =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut second_store = SessionStore::open_managed(&sessions, second.clone()).expect("store");
    let error = second_store
        .prepare_session("managed-session", SessionPreparationMode::Resume)
        .expect_err("second owner lock rejects");
    assert!(
        error.to_string().contains("unavailable")
            || error
                .to_string()
                .contains("Resource temporarily unavailable")
    );
    assert_eq!(second.ledger_for_test().2, 0);
}

/// Final release rejects stale leases and returns stream capacity for repeated
/// same-identity generations.
#[test]
fn repeated_release_reuses_capacity_and_rejects_stale_generation() {
    let root = tempfile::tempdir().expect("temporary root");
    let owner = Arc::new(
        SemanticPersistenceOwner::new(PersistenceCapacity {
            max_frames: 8,
            max_bytes: 256 * 1024 * 1024,
            max_streams: 2,
        })
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    let mut stale = None;
    for _ in 0..16 {
        store
            .prepare_session("managed-session", SessionPreparationMode::New)
            .expect("generation preparation reuses stream permits");
        let leases = store.managed_persistence_leases("managed-session");
        stale.get_or_insert_with(|| leases[0].clone());
        owner
            .release(&leases, Duration::from_secs(2))
            .expect("release generation");
        store.finish_managed_release("managed-session");
    }
    let error = match stale.expect("stale lease").try_reserve_frame() {
        Ok(_) => panic!("released generation unexpectedly admitted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        super::PersistenceAdmissionError::StaleLease
    ));
}

/// Retiring a superseded complete projection must not hold the global admission
/// mutex, and releasing its staging bytes must wake a concurrent byte-Full
/// publisher without waiting for worker traffic.
#[test]
fn held_projection_retirement_releases_capacity_outside_admission_lock() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let capacity = PersistenceCapacity {
        max_frames: 8,
        max_bytes: 256 * 1024 * 1024,
        max_streams: 4,
    };
    let charge = RetentionCharge {
        frame: 8,
        replacement: 8 * 1024 * 1024,
        checkpoint: 0,
        projections: 0,
    };
    let (prepared_bytes, first_total) = {
        let owner = Arc::new(SemanticPersistenceOwner::new(capacity).expect("probe owner"));
        let mut store = SessionStore::open_managed(&sessions, owner.clone()).expect("probe store");
        store
            .prepare_session("held-retirement", SessionPreparationMode::New)
            .expect("probe prepare");
        let prepared = owner.ledger_for_test().1;
        let lease = store.managed_persistence_leases("held-retirement")[0].clone();
        let staging = lease
            .try_reserve_frame()
            .expect("probe frame")
            .reserve_bytes(charge)
            .expect("probe bytes");
        let total = owner.ledger_for_test().1 - prepared;
        drop(staging);
        owner
            .release(
                &store.managed_persistence_leases("held-retirement"),
                Duration::from_secs(2),
            )
            .expect("probe release");
        (prepared, total)
    };
    let backend = Arc::new(WriteFaultBackend::new());
    backend.hold_writes.store(true, Ordering::SeqCst);
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity {
                max_bytes: prepared_bytes + first_total + 512 * 1024,
                ..capacity
            },
            backend.clone(),
        )
        .expect("bounded owner"),
    );
    let mut store = SessionStore::open_managed(&sessions, owner.clone()).expect("store");
    store
        .prepare_session("held-retirement", SessionPreparationMode::Resume)
        .expect("resume");
    let lease = store.managed_persistence_leases("held-retirement")[0].clone();
    let staging = lease
        .try_reserve_frame()
        .expect("held frame")
        .reserve_bytes(charge)
        .expect("held bytes");
    let drop_state = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let target = HeldProjection {
        bytes: vec![0; charge.replacement],
        hold_drop: true,
        drop_state: Arc::clone(&drop_state),
    };
    let replacement = HeldProjection {
        bytes: vec![1],
        hold_drop: false,
        drop_state: Arc::clone(&drop_state),
    };
    let (commit_tx, commit_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut target = target;
        let result = staging.commit_swap(
            &mut target,
            replacement,
            StagedFrame::ordinary(Vec::new(), None),
        );
        let _ = commit_tx.send(result);
    });
    {
        let (state, wake) = &*drop_state;
        let state = state.lock().expect("drop state");
        let (state, _) = wake
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.0)
            .expect("drop-start wait");
        assert!(state.0, "superseded projection did not enter Drop");
    }
    backend.wait_until_write_held();

    let (admission_tx, admission_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = lease
            .try_reserve_frame()
            .and_then(|frame| {
                frame.reserve_bytes(RetentionCharge {
                    frame: 8,
                    replacement: 1024 * 1024,
                    checkpoint: 0,
                    projections: 0,
                })
            })
            .map(drop);
        let _ = admission_tx.send(result);
    });
    let error = admission_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("independent admission must not wait for projection Drop")
        .expect_err("retained staging owns the remaining byte capacity");
    assert!(matches!(error, super::PersistenceAdmissionError::Full));
    assert_eq!(
        owner
            .drain_operational_status()
            .capacity_full
            .expect("byte pressure")
            .limit,
        super::PersistenceCapacityLimit::Bytes
    );
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    owner.set_operational_wake(Arc::new(move || {
        let _ = wake_tx.try_send(());
    }));
    {
        let (state, wake) = &*drop_state;
        let mut state = state.lock().expect("drop state");
        state.1 = true;
        wake.notify_all();
    }
    commit_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("commit returns after retirement")
        .expect("commit");
    wake_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("retirement capacity recovery wake");
    assert!(owner.drain_operational_status().recovered.is_some());
    backend.release_writes();
}

/// Aggregate registry/path/handle/root-target capacity rejects at one byte
/// below the exact two-stream boundary and restores every partial reservation.
#[test]
fn exact_stream_byte_boundary_restores_partial_registration() {
    let root = tempfile::tempdir().expect("temporary root");
    let sessions = root.path().join("sessions");
    let capacity = PersistenceCapacity {
        max_frames: 8,
        max_bytes: 256 * 1024 * 1024,
        max_streams: 2,
    };
    let exact_bytes;
    let baseline;
    {
        let owner = Arc::new(SemanticPersistenceOwner::new(capacity).expect("probe owner"));
        baseline = owner.ledger_for_test().1;
        let mut store = SessionStore::open_managed(&sessions, owner.clone()).expect("store");
        store
            .prepare_session("managed-session", SessionPreparationMode::New)
            .expect("probe preparation");
        exact_bytes = owner.ledger_for_test().1;
        owner
            .release(
                &store.managed_persistence_leases("managed-session"),
                Duration::from_secs(2),
            )
            .expect("probe release");
    }
    {
        let owner = Arc::new(
            SemanticPersistenceOwner::new(PersistenceCapacity {
                max_bytes: exact_bytes - 1,
                ..capacity
            })
            .expect("owner below stream boundary"),
        );
        let mut store = SessionStore::open_managed(&sessions, owner.clone()).expect("store");
        let error = store
            .prepare_session("managed-session", SessionPreparationMode::Resume)
            .expect_err("one byte below exact aggregate rejects");
        assert!(error.to_string().contains("full"));
        assert_eq!(owner.ledger_for_test(), (0, baseline, 0));
    }
    {
        let owner = Arc::new(
            SemanticPersistenceOwner::new(PersistenceCapacity {
                max_bytes: exact_bytes,
                ..capacity
            })
            .expect("owner at exact boundary"),
        );
        let mut store = SessionStore::open_managed(&sessions, owner.clone()).expect("store");
        store
            .prepare_session("managed-session", SessionPreparationMode::Resume)
            .expect("exact aggregate boundary accepts");
        assert_eq!(owner.ledger_for_test().1, exact_bytes);
        owner
            .release(
                &store.managed_persistence_leases("managed-session"),
                Duration::from_secs(2),
            )
            .expect("exact boundary release");
        assert_eq!(owner.ledger_for_test(), (0, baseline, 0));
    }
}

/// Agent checkpoint debt covers the exact written frame and survives clean
/// release as a strict watermarked sidecar.
#[test]
fn agent_checkpoint_is_published_before_clean_release() {
    let root = tempfile::tempdir().expect("temporary root");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let agents = root.path().join("agents");
    let mut store = AgentStore::open_managed(&agents, owner.clone()).expect("store");
    let agent_id = tau_proto::AgentId::parse("checkpoint-agent").expect("agent id");
    store
        .reserve_new_agent(agent_id.as_str())
        .expect("reserve agent");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            started_event(&agent_id),
            tau_proto::UnixMicros::new(7),
        )
        .expect("append creation");
    let checkpoint_path = agents.join(agent_id.as_str()).join("meta.json");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !checkpoint_path.exists() {
        assert!(
            Instant::now() < deadline,
            "managed checkpoint was not published while its writer remained live"
        );
        std::thread::yield_now();
    }
    let snapshot = crate::AgentJournalSnapshot::capture(&agents, [agent_id.clone()])
        .expect("live reader accepts managed producer checkpoint");
    assert_eq!(
        snapshot
            .records(&agent_id)
            .expect("managed agent reader")
            .count(),
        1
    );
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            display_name_event(&agent_id, "after-live-read"),
            tau_proto::UnixMicros::new(8),
        )
        .expect("managed writer survives live checkpoint read");
    let leases = store.managed_persistence_leases();
    owner
        .release(&leases, Duration::from_secs(2))
        .expect("release drains checkpoint debt");
    let checkpoint: crate::AgentCheckpoint =
        serde_json::from_slice(&std::fs::read(checkpoint_path).expect("checkpoint sidecar"))
            .expect("decode checkpoint");
    assert_eq!(checkpoint.agent_id, agent_id);
    assert_eq!(
        checkpoint.journal.next_seq,
        crate::PersistedAgentEventSeq::new(2)
    );
    assert!(checkpoint.journal.covered_bytes > 8);
}

/// Selected maintenance snapshot relinquishes and disposes only selected
/// generations; unrelated agents remain live-admissible.
#[test]
fn selected_snapshot_leaves_unrelated_agent_generation_live() {
    let root = tempfile::tempdir().expect("temporary root");
    let owner =
        Arc::new(SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("owner"));
    let mut store = AgentStore::open_managed(root.path().join("agents"), owner).expect("store");
    let selected = tau_proto::AgentId::parse("selected-agent").expect("agent id");
    let unrelated = tau_proto::AgentId::parse("unrelated-agent").expect("agent id");
    for agent_id in [&selected, &unrelated] {
        store.reserve_new_agent(agent_id.as_str()).expect("reserve");
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                crate::AgentEventParent::InheritHead,
                started_event(agent_id),
                tau_proto::UnixMicros::new(7),
            )
            .expect("append creation");
    }
    let snapshot = store
        .capture_managed_snapshot([selected.clone()], Duration::from_secs(2))
        .expect("selected snapshot");
    assert_eq!(
        snapshot
            .records(&selected)
            .expect("selected records")
            .collect::<Result<Vec<_>, _>>()
            .expect("read selected records")
            .len(),
        1
    );
    store
        .append_agent_event_at(
            unrelated.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            tau_proto::Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: unrelated.clone(),
                display_name: "still live".to_owned(),
            }),
            tau_proto::UnixMicros::new(8),
        )
        .expect("unrelated generation remains live");
}

/// A mid-frame failure rolls back to exact EOF and retries the same FIFO head.
#[test]
fn exact_prefix_write_failure_retries_without_duplicate_frame() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let mut store =
        SessionStore::open_managed(root.path().join("sessions"), owner.clone()).expect("store");
    let wakes = Arc::new(AtomicUsize::new(0));
    let wake_count = Arc::clone(&wakes);
    let (wake_tx, wake_rx) = mpsc::sync_channel(4);
    owner.set_operational_wake(Arc::new(move || {
        wake_count.fetch_add(1, Ordering::SeqCst);
        let _ = wake_tx.try_send(());
    }));
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    backend.fail_writes.store(true, Ordering::SeqCst);
    store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(7),
        )
        .expect("live fact remains accepted");

    wake_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("first write failure wake");
    assert!(
        owner
            .drain_failures()
            .iter()
            .any(|failure| failure.kind() == PersistenceFailureKind::Write)
    );
    wake_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("legacy drain rearms the next write failure wake");
    assert_eq!(
        wakes.load(Ordering::SeqCst),
        2,
        "each legacy drain rearms exactly one coalesced operational wake"
    );
    backend.fail_writes.store(false, Ordering::SeqCst);
    let leases = store.managed_persistence_leases("managed-session");
    owner
        .release(&leases, Duration::from_secs(2))
        .expect("retry and durability debt drain before release");
    store.finish_managed_release("managed-session");
    drop(store);
    drop(owner);

    let owner = Arc::new(
        SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("restart owner"),
    );
    let mut reopened =
        SessionStore::open_managed(root.path().join("sessions"), owner).expect("restart store");
    reopened
        .prepare_session("managed-session", SessionPreparationMode::Resume)
        .expect("strict resume");
    assert_eq!(
        reopened
            .session_events("managed-session")
            .expect("replayed events")
            .len(),
        1
    );
}

/// Rollback-safe ENOSPC must stop the global FIFO at its head, fill the exact
/// byte ledger rather than the frame limit, roll back a rejected append, and
/// recover the accepted live prefix identically after cold replay.
#[test]
fn rollback_safe_enospc_byte_boundary_recovers_fifo_and_replay() {
    let root = tempfile::tempdir().expect("temporary root");
    let capacity = PersistenceCapacity {
        max_frames: 64,
        max_bytes: 256 * 1024 * 1024,
        max_streams: 8,
    };
    let agent_id = tau_proto::AgentId::parse("byte-ledger-agent").expect("agent id");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(capacity, backend.clone())
            .expect("boundary owner"),
    );
    let baseline = owner.ledger_for_test().1;
    let agents_path = root.path().join("store-a");
    let sessions_path = root.path().join("store-s");
    let mut agents = AgentStore::open_managed(&agents_path, owner.clone()).expect("agents");
    let mut sessions = SessionStore::open_managed(&sessions_path, owner.clone()).expect("sessions");
    sessions
        .prepare_session("byte-ledger-session", SessionPreparationMode::New)
        .expect("prepare session");
    agents
        .reserve_new_agent(agent_id.as_str())
        .expect("reserve agent");
    backend
        .fail_writes_with_enospc
        .store(true, Ordering::SeqCst);
    agents
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            started_event(&agent_id),
            tau_proto::UnixMicros::new(1),
        )
        .expect("admit failed head");
    assert!(
        owner.wait_for_failure_for_test(PersistenceFailureKind::Write, Duration::from_secs(2),)
    );
    sessions
        .append_session_event_at(
            "byte-ledger-session",
            None,
            loaded_event("byte-ledger-session"),
            tau_proto::UnixMicros::new(2),
        )
        .expect("admit session behind head");
    agents
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            tau_proto::Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                agent_id: agent_id.clone(),
                display_name: "accepted".to_owned(),
            }),
            tau_proto::UnixMicros::new(3),
        )
        .expect("admit later agent frame");
    let before_filler = owner.ledger_for_test().1;
    let lease = agents.managed_persistence_leases()[0].clone();
    let internal_probe = lease
        .try_reserve_frame()
        .expect("probe filler frame")
        .reserve_bytes(RetentionCharge {
            frame: 0,
            replacement: 0,
            checkpoint: 0,
            projections: 0,
        })
        .expect("probe filler bytes");
    let internal_bytes = owner.ledger_for_test().1 - before_filler;
    drop(internal_probe);
    let filler = lease
        .try_reserve_frame()
        .expect("exact filler frame")
        .reserve_bytes(RetentionCharge {
            frame: capacity.max_bytes - before_filler - internal_bytes,
            replacement: 0,
            checkpoint: 0,
            projections: 0,
        })
        .expect("fill exact byte boundary");
    assert_eq!(owner.ledger_for_test(), (4, capacity.max_bytes, 3));
    let before = sessions
        .session_events("byte-ledger-session")
        .expect("live session")
        .to_vec();
    let rejected = sessions.append_session_event_at(
        "byte-ledger-session",
        None,
        loaded_event("byte-ledger-session"),
        tau_proto::UnixMicros::new(4),
    );
    assert!(matches!(
        rejected,
        Err(crate::SessionStoreError::Persistence(
            super::PersistenceAdmissionError::Full
        ))
    ));
    let pressure = owner
        .drain_operational_status()
        .capacity_full
        .expect("byte pressure transition");
    assert_eq!(pressure.limit, super::PersistenceCapacityLimit::Bytes);
    assert_eq!(pressure.usage.bytes, capacity.max_bytes);
    assert_eq!(pressure.usage.frames, 4);
    assert_eq!(
        sessions
            .session_events("byte-ledger-session")
            .expect("live session"),
        before
    );
    assert_eq!(owner.ledger_for_test(), (4, capacity.max_bytes, 3));
    assert!(
        std::fs::metadata(sessions_path.join("byte-ledger-session/events.cbor"))
            .expect("session journal")
            .len()
            == 0,
        "later stream must not overtake the failed head"
    );

    drop(filler);
    backend
        .fail_writes_with_enospc
        .store(false, Ordering::SeqCst);
    let mut leases = agents.managed_persistence_leases();
    leases.extend(sessions.managed_persistence_leases("byte-ledger-session"));
    owner
        .release(&leases, Duration::from_secs(2))
        .expect("recovery drains FIFO");
    agents.finish_managed_release();
    sessions.finish_managed_release("byte-ledger-session");
    assert_eq!(owner.ledger_for_test(), (0, baseline, 0));
    let recovered = owner.drain_operational_status();
    assert!(recovered.recovered.is_some());
    assert_eq!(recovered.drained.expect("drained transition").frames, 0);
    drop(agents);
    drop(sessions);
    drop(owner);

    let owner = Arc::new(
        SemanticPersistenceOwner::new(PersistenceCapacity::default()).expect("cold owner"),
    );
    let mut cold_agents =
        AgentStore::open_managed(&agents_path, owner.clone()).expect("cold agents");
    let mut cold_sessions =
        SessionStore::open_managed(&sessions_path, owner).expect("cold sessions");
    cold_agents
        .prepare_existing_agent(agent_id.as_str())
        .expect("cold agent replay");
    cold_sessions
        .prepare_session("byte-ledger-session", SessionPreparationMode::Resume)
        .expect("cold session replay");
    let cold_agent = cold_agents.agent(agent_id.as_str()).expect("cold agent");
    assert_eq!(cold_agent.display_name(), Some("accepted"));
    assert_eq!(
        cold_sessions
            .session_events("byte-ledger-session")
            .expect("cold session"),
        before
    );
}

/// Healthy small-event bursts behind a slow worker retain only frame, delta,
/// checkpoint, and worker ownership; they must not reserve four encoded copies
/// of the complete live projection for every queued frame.
#[test]
fn slow_healthy_worker_does_not_amplify_complete_projection_per_frame() {
    let root = tempfile::tempdir().expect("temporary root");
    let backend = Arc::new(WriteFaultBackend::new());
    let owner = Arc::new(
        SemanticPersistenceOwner::with_test_backend(
            PersistenceCapacity::default(),
            backend.clone(),
        )
        .expect("owner"),
    );
    let agent_id = tau_proto::AgentId::parse("healthy-burst-agent").expect("agent id");
    let mut store =
        AgentStore::open_managed(root.path().join("agents"), owner.clone()).expect("store");
    store
        .reserve_new_agent(agent_id.as_str())
        .expect("reserve agent");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            started_event(&agent_id),
            tau_proto::UnixMicros::new(1),
        )
        .expect("create agent");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            tau_proto::Event::AgentUserMessageInjected(tau_proto::AgentUserMessageInjected {
                agent_id: agent_id.clone(),
                text: "x".repeat(1024 * 1024),
                inference_activation: false,
                message_class: Default::default(),
            }),
            tau_proto::UnixMicros::new(2),
        )
        .expect("build large live projection");
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_secs(2)),
        DurabilityBarrierOutcome::Durable
    );
    let before_burst = owner.ledger_for_test().1;

    backend.hold_writes.store(true, Ordering::SeqCst);
    for ordinal in 0..64 {
        store
            .append_agent_event_at(
                agent_id.as_str(),
                None,
                crate::AgentEventParent::InheritHead,
                tau_proto::Event::AgentDisplayNameSet(tau_proto::AgentDisplayNameSet {
                    agent_id: agent_id.clone(),
                    display_name: format!("healthy-{ordinal}"),
                }),
                tau_proto::UnixMicros::new(3 + ordinal),
            )
            .expect("healthy burst must fit actual retained ownership");
        if ordinal == 0 {
            backend.wait_until_write_held();
        }
    }
    let burst = owner.ledger_for_test();
    assert_eq!(burst.0, 64);
    assert!(
        burst.1 - before_burst < 16 * 1024 * 1024,
        "small queued deltas retained {} bytes above baseline",
        burst.1 - before_burst
    );
    assert!(
        owner.drain_operational_status().capacity_full.is_none(),
        "healthy burst must not hit byte pressure"
    );
    assert_eq!(
        owner.wait_for_latest_durability_for_test(Duration::from_millis(1)),
        DurabilityBarrierOutcome::DeadlineExpired,
        "held write must preserve the test barrier's deadline classification"
    );

    backend.release_writes();
    owner
        .release(&store.managed_persistence_leases(), Duration::from_secs(2))
        .expect("healthy burst drains");
    store.finish_managed_release();
}

/// Every length-prefix and payload offset proves exact rollback before retry;
/// no offset duplicates or skips the frame. This exhaustive oracle calls the
/// production frame primitive directly, while
/// `exact_prefix_write_failure_retries_without_duplicate_frame` owns the
/// asynchronous FIFO integration boundary.
#[test]
fn every_frame_write_offset_rolls_back_and_retries_exactly_once() {
    let backend = WriteFaultBackend::new();
    let mut journal = tempfile::tempfile().expect("temporary journal");
    let existing = b"existing-frame";
    journal.write_all(existing).expect("seed journal");
    let record = crate::PersistedSessionEvent {
        seq: crate::PersistedSessionEventSeq::new(0),
        source: None,
        event: loaded_event("managed-session"),
        recorded_at: tau_proto::UnixMicros::new(7),
    };
    let mut payload = Vec::new();
    ciborium::into_writer(&record, &mut payload).expect("encode expected frame");

    let mut run = |write_call: usize, offset: usize| {
        journal
            .set_len(u64::try_from(existing.len()).expect("existing length"))
            .expect("reset journal");
        backend.inject_short_write(write_call, offset);
        assert_eq!(
            super::worker::append_frame(&backend, &mut journal, &payload, || {}),
            Err(super::worker::FrameAppendError::RolledBack),
            "write {write_call} offset {offset} must roll back"
        );
        journal.rewind().expect("rewind rolled-back journal");
        let mut bytes = Vec::new();
        journal
            .read_to_end(&mut bytes)
            .expect("read rolled-back journal");
        assert_eq!(bytes, existing);

        let end = super::worker::append_frame(&backend, &mut journal, &payload, || {
            panic!("successful retry must not report a write failure")
        })
        .expect("same frame retries");
        assert_eq!(
            end,
            u64::try_from(existing.len() + 8 + payload.len()).expect("frame end")
        );
        journal.rewind().expect("rewind retried journal");
        bytes.clear();
        journal
            .read_to_end(&mut bytes)
            .expect("read retried journal");
        assert_eq!(&bytes[..existing.len()], existing);
        assert_eq!(
            u64::from_le_bytes(
                bytes[existing.len()..existing.len() + 8]
                    .try_into()
                    .expect("length prefix")
            ) as usize,
            payload.len()
        );
        assert_eq!(&bytes[existing.len() + 8..], payload);
    };

    for offset in 0..=8 {
        run(1, offset);
    }
    for offset in 0..=payload.len() {
        run(2, offset);
    }
}

/// A write failure becomes observable before rollback touches the journal.
#[test]
fn frame_write_failure_is_observed_before_rollback() {
    let backend = WriteFaultBackend::new();
    let observed = Arc::new(AtomicBool::new(false));
    backend.require_observation_before_seek(2, Arc::clone(&observed));
    backend.inject_short_write(1, 3);
    let mut journal = tempfile::tempfile().expect("temporary journal");
    let callback_observed = Arc::clone(&observed);

    assert_eq!(
        super::worker::append_frame(&backend, &mut journal, b"payload", move || {
            callback_observed.store(true, Ordering::SeqCst);
        }),
        Err(super::worker::FrameAppendError::RolledBack)
    );
    assert!(observed.load(Ordering::SeqCst));
}

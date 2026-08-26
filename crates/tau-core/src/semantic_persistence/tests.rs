//! Deterministic production-backend persistence failure oracles.

use std::fs::{File, Permissions};
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::backend::{FilesystemBackend, PersistenceBackend};
use super::{PersistenceCapacity, PersistenceFailureKind, SemanticPersistenceOwner};
use crate::{AgentStore, SessionPreparationMode, SessionStore};

/// One-shot exact-prefix write fault over the production filesystem backend.
struct WriteFaultBackend {
    fail_next_write: AtomicBool,
    fail_next_truncate: AtomicBool,
    fail_writes: AtomicBool,
    exit_next_write: AtomicBool,
    fail_next_sync_data: AtomicBool,
    fail_next_sync_all: AtomicBool,
    fail_next_rename: AtomicBool,
    fail_next_lock: AtomicBool,
    journal_writes: Mutex<Vec<String>>,
    renames: Mutex<usize>,
    rename_wake: Condvar,
    write_call: AtomicUsize,
    short_write: Mutex<Option<(usize, usize)>>,
    fail_next_directory_create: AtomicBool,
    create_file_call: AtomicUsize,
    fail_create_file_call: AtomicUsize,
    failed_sync_path: Mutex<Option<String>>,
}

impl WriteFaultBackend {
    fn new() -> Self {
        Self {
            fail_next_write: AtomicBool::new(false),
            fail_next_truncate: AtomicBool::new(false),
            fail_writes: AtomicBool::new(false),
            exit_next_write: AtomicBool::new(false),
            fail_next_sync_data: AtomicBool::new(false),
            fail_next_sync_all: AtomicBool::new(false),
            fail_next_rename: AtomicBool::new(false),
            fail_next_lock: AtomicBool::new(false),
            journal_writes: Mutex::new(Vec::new()),
            renames: Mutex::new(0),
            rename_wake: Condvar::new(),
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

    fn inject_short_write(&self, call: usize, offset: usize) {
        self.write_call.store(0, Ordering::SeqCst);
        *self.short_write.lock().expect("short-write fault") = Some((call, offset));
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
    fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()> {
        FilesystemBackend.set_permissions(path, permissions)
    }
    fn create_new_file(&self, path: &Path) -> io::Result<File> {
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
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        FilesystemBackend.remove_file(path)
    }
    fn open_directory(&self, path: &Path) -> io::Result<File> {
        FilesystemBackend.open_directory(path)
    }
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        FilesystemBackend.read_file(path)
    }
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
    assert!(
        owner
            .wait_for_failure_for_test(PersistenceFailureKind::WorkerExit, Duration::from_secs(2),)
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

/// Unprovable rollback poisons only the affected generation in memory.
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
    assert!(
        owner.wait_for_failure_for_test(PersistenceFailureKind::Rollback, Duration::from_secs(2),)
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

/// A new-agent disk collision diagnoses the accepted generation without
/// truncating or deleting pre-existing canonical bytes.
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
    let mut store = AgentStore::open_managed(&agents, owner.clone()).expect("store");
    let agent_id = tau_proto::AgentId::parse("collision-agent").expect("agent id");
    store
        .reserve_new_agent(agent_id.as_str())
        .expect("in-memory reservation ignores disk");
    store
        .append_agent_event_at(
            agent_id.as_str(),
            None,
            crate::AgentEventParent::InheritHead,
            started_event(&agent_id),
            tau_proto::UnixMicros::new(7),
        )
        .expect("creation fact accepted before asynchronous collision");
    assert!(
        owner.wait_for_failure_for_test(PersistenceFailureKind::Collision, Duration::from_secs(2),)
    );
    assert_eq!(std::fs::read(journal).expect("old journal"), old_bytes);
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
    assert!(
        !owner.wait_for_latest_durability_for_test(Duration::from_millis(50)),
        "later stream debt must not satisfy a barrier over earlier failed debt"
    );
    *backend.failed_sync_path.lock().expect("failed sync path") = None;
    assert!(owner.wait_for_latest_durability_for_test(Duration::from_secs(2)));
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
    let leases = store.managed_persistence_leases();
    owner
        .release(&leases, Duration::from_secs(2))
        .expect("release drains checkpoint debt");
    let checkpoint: crate::AgentCheckpoint = serde_json::from_slice(
        &std::fs::read(agents.join(agent_id.as_str()).join("meta.json"))
            .expect("checkpoint sidecar"),
    )
    .expect("decode checkpoint");
    assert_eq!(checkpoint.agent_id, agent_id);
    assert_eq!(checkpoint.journal.next_seq, 1);
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
    store
        .prepare_session("managed-session", SessionPreparationMode::New)
        .expect("prepare");
    backend.fail_next_write.store(true, Ordering::SeqCst);
    store
        .append_session_event_at(
            "managed-session",
            None,
            loaded_event("managed-session"),
            tau_proto::UnixMicros::new(7),
        )
        .expect("live fact remains accepted");

    assert!(
        owner.wait_for_failure_for_test(PersistenceFailureKind::Write, Duration::from_secs(2),)
    );
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

/// Every length-prefix and payload offset proves exact rollback before the same
/// FIFO node retries; no offset duplicates or skips the frame.
#[test]
fn every_frame_write_offset_rolls_back_and_retries_exactly_once() {
    let event = loaded_event("managed-session");
    let record = crate::PersistedSessionEvent {
        seq: crate::PersistedSessionEventSeq::new(0),
        source: None,
        event: event.clone(),
        recorded_at: tau_proto::UnixMicros::new(7),
    };
    let mut expected_payload = Vec::new();
    ciborium::into_writer(&record, &mut expected_payload).expect("encode expected frame");

    let run = |write_call: usize, offset: usize| {
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
        backend.inject_short_write(write_call, offset);
        store
            .append_session_event_at(
                "managed-session",
                None,
                event.clone(),
                tau_proto::UnixMicros::new(7),
            )
            .expect("live append accepted");
        assert!(
            owner.wait_for_failure_for_test(PersistenceFailureKind::Write, Duration::from_secs(2),)
        );
        owner
            .release(
                &store.managed_persistence_leases("managed-session"),
                Duration::from_secs(2),
            )
            .expect("retry drains before release");
        let bytes =
            std::fs::read(sessions.join("managed-session/events.cbor")).expect("journal bytes");
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().expect("length prefix")) as usize,
            expected_payload.len()
        );
        assert_eq!(&bytes[8..], expected_payload);
    };

    for offset in 0..=8 {
        run(1, offset);
    }
    for offset in 0..=expected_payload.len() {
        run(2, offset);
    }
}

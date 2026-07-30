use std::collections::HashSet;
use std::sync as path_std_sync;
use std::sync::Condvar;
use std::time::{Duration, Instant};

use super::*;

#[derive(Default)]
/// Deterministic backend with observable calls, failures, and blocking.
struct TestBackend {
    /// Mutable call/failure plan.
    state: Mutex<TestState>,
    /// Signals new calls and releases a blocked file sync.
    wake: Condvar,
}

#[derive(Default)]
/// Mutable deterministic backend plan and observations.
struct TestState {
    /// Ordered filesystem-operation trace.
    calls: Vec<(char, PathBuf)>,
    /// File paths whose sync fails.
    failed_files: HashSet<PathBuf>,
    /// Directory paths whose sync fails.
    failed_directories: HashSet<PathBuf>,
    /// Optional file path held inside sync.
    blocked_file: Option<PathBuf>,
    /// Whether the worker entered the configured block.
    blocked_entered: bool,
    /// Whether the blocked worker may continue.
    release_block: bool,
}

impl TestBackend {
    fn wait_for(&self, predicate: impl Fn(&TestState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = self.state.lock().expect("test backend lock");
        while !predicate(&state) {
            let timeout = deadline.saturating_duration_since(Instant::now());
            assert!(!timeout.is_zero(), "timed out waiting for sync calls");
            (state, _) = self
                .wake
                .wait_timeout(state, timeout)
                .expect("test backend wait");
        }
    }

    fn calls(&self) -> Vec<(char, PathBuf)> {
        self.state.lock().expect("test backend lock").calls.clone()
    }
}

impl SyncBackend for TestBackend {
    fn sync_file(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().expect("test backend lock");
        state.calls.push(('f', path.to_path_buf()));
        if state.blocked_file.as_deref() == Some(path) && !state.release_block {
            state.blocked_entered = true;
            self.wake.notify_all();
            while !state.release_block {
                state = self.wake.wait(state).expect("test backend wait");
            }
        }
        self.wake.notify_all();
        if state.failed_files.contains(path) {
            Err(io::Error::other("injected file sync failure"))
        } else {
            Ok(())
        }
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().expect("test backend lock");
        state.calls.push(('d', path.to_path_buf()));
        self.wake.notify_all();
        if state.failed_directories.contains(path) {
            Err(io::Error::other("injected directory sync failure"))
        } else {
            Ok(())
        }
    }
}

/// Dirty marking stays coalesced and does not panic when worker startup
/// fails.
#[test]
fn spawn_failure_preserves_one_dirty_state() {
    let backend = Arc::new(TestBackend::default());
    let worker = JournalSyncWorker::with_backend(backend);
    worker.inject_spawn_failure();
    let path = Path::new("/tmp/a/events.cbor");

    worker.mark_dirty(path, 10, std::iter::empty());
    worker.mark_dirty(path, 20, [PathBuf::from("/tmp/a")]);
    {
        let state = worker.shared.state.lock().expect("worker state");
        let target = state.dirty.get(path).expect("racing target");
        assert_eq!(target.end_offset, 20);
        assert_eq!(
            target.directories,
            [PathBuf::from("/tmp/a")].into_iter().collect()
        );
    }

    assert!(
        worker
            .spawn_attempted
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    let state = worker.shared.state.lock().expect("worker state");
    assert_eq!(state.dirty.len(), 1);
    assert_eq!(state.ready.len(), 1);
    let dirty = state.dirty.get(path).expect("dirty journal");
    assert_eq!(dirty.end_offset, 20);
    assert_eq!(dirty.generation, 2);
}

/// A sync racing a later write must run again across generation rollover.
#[test]
fn racing_generation_is_resynced_without_blocking_mark() {
    let backend = Arc::new(TestBackend::default());
    backend
        .state
        .lock()
        .expect("test backend lock")
        .blocked_file = Some(PathBuf::from("/tmp/a/events.cbor"));
    let worker = JournalSyncWorker::with_backend(backend.clone());
    worker
        .shared
        .state
        .lock()
        .expect("worker state")
        .next_generation = u64::MAX;
    let path = Path::new("/tmp/a/events.cbor");
    worker.mark_dirty(path, 10, std::iter::empty());
    backend.wait_for(|state| state.blocked_entered);

    worker.mark_dirty(path, 20, [PathBuf::from("/tmp/a")]);
    {
        let state = worker.shared.state.lock().expect("worker state");
        let target = state.dirty.get(path).expect("raced target");
        assert_eq!(target.generation, 1);
        assert_eq!(target.end_offset, 20);
        assert_eq!(
            target.directories,
            [PathBuf::from("/tmp/a")].into_iter().collect()
        );
    }
    {
        let mut state = backend.state.lock().expect("test backend lock");
        state.release_block = true;
        backend.wake.notify_all();
    }
    backend.wait_for(|state| {
        state
            .calls
            .iter()
            .filter(|(kind, called)| *kind == 'f' && called == path)
            .count()
            == 2
    });
    backend.wait_for(|state| state.calls.contains(&('d', PathBuf::from("/tmp/a"))));
}

/// Store shutdown detaches rather than joining a blocked sync attempt.
#[test]
fn shutdown_does_not_join_blocked_sync() {
    let backend = Arc::new(TestBackend::default());
    backend
        .state
        .lock()
        .expect("test backend lock")
        .blocked_file = Some(PathBuf::from("/tmp/a/events.cbor"));
    let worker = JournalSyncWorker::with_backend(backend.clone());
    worker.mark_dirty(Path::new("/tmp/a/events.cbor"), 10, std::iter::empty());
    backend.wait_for(|state| state.blocked_entered);
    let (sent, received) = path_std_sync::mpsc::channel();

    thread::spawn(move || {
        drop(worker);
        sent.send(()).expect("report dropped worker");
    });

    received
        .recv_timeout(Duration::from_secs(1))
        .expect("shutdown must not join sync");
    let mut state = backend.state.lock().expect("test backend lock");
    state.release_block = true;
    backend.wake.notify_all();
}

/// A persistently failing lexical-first path cannot starve another journal.
#[test]
fn failed_path_rotates_behind_other_dirty_paths() {
    let backend = Arc::new(TestBackend::default());
    backend
        .state
        .lock()
        .expect("test backend lock")
        .failed_files
        .extend([PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]);
    let worker = JournalSyncWorker::with_backend(backend.clone());

    worker.mark_dirty(Path::new("/tmp/a"), 10, std::iter::empty());
    worker.mark_dirty(Path::new("/tmp/b"), 10, std::iter::empty());
    worker.mark_dirty(Path::new("/tmp/c"), 10, std::iter::empty());

    backend.wait_for(|state| {
        state
            .calls
            .iter()
            .any(|(kind, path)| *kind == 'f' && path == Path::new("/tmp/c"))
    });
    let calls = backend.calls();
    let a = calls
        .iter()
        .position(|call| call == &('f', PathBuf::from("/tmp/a")))
        .expect("a attempted");
    let c = calls
        .iter()
        .position(|call| call == &('f', PathBuf::from("/tmp/c")))
        .expect("c attempted");
    assert!(a < c);
}

/// File sync precedes child-to-parent directory sync, and a failed child
/// directory prevents false parent coverage.
#[test]
fn file_and_directory_coverage_is_ordered() {
    let backend = TestBackend::default();
    backend
        .state
        .lock()
        .expect("test backend lock")
        .failed_directories
        .insert(PathBuf::from("/tmp/a"));
    let target = DirtyJournal {
        kind: SyncTargetKind::Journal,
        generation: 1,
        end_offset: 10,
        directories: [
            PathBuf::from("/tmp/a/b"),
            PathBuf::from("/tmp/a"),
            PathBuf::from("/tmp"),
        ]
        .into_iter()
        .collect(),
        retry_at: None,
        failures: 0,
    };

    sync_target(&backend, Path::new("/tmp/a/b/events.cbor"), &target)
        .expect_err("directory sync fails");

    assert_eq!(
        backend.calls(),
        vec![
            ('f', PathBuf::from("/tmp/a/b/events.cbor")),
            ('d', PathBuf::from("/tmp/a/b")),
            ('d', PathBuf::from("/tmp/a")),
        ]
    );
}

/// Clearing a partial directory failure and notifying new work retries the
/// whole ordered target and eventually clears its dirty state.
#[test]
fn partial_directory_failure_retries_to_clean() {
    let backend = Arc::new(TestBackend::default());
    let directory = PathBuf::from("/tmp/a");
    backend
        .state
        .lock()
        .expect("test backend lock")
        .failed_directories
        .insert(directory.clone());
    let worker = JournalSyncWorker::with_backend(backend.clone());
    let path = Path::new("/tmp/a/events.cbor");
    worker.mark_dirty(path, 10, [directory.clone()]);
    backend.wait_for(|state| {
        state
            .calls
            .iter()
            .any(|call| call == &('d', directory.clone()))
    });
    backend
        .state
        .lock()
        .expect("test backend lock")
        .failed_directories
        .clear();

    backend.wait_for(|state| {
        2 <= state
            .calls
            .iter()
            .filter(|call| call == &&('d', directory.clone()))
            .count()
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while worker
        .shared
        .state
        .lock()
        .expect("worker state")
        .dirty
        .contains_key(path)
    {
        assert!(Instant::now() < deadline, "dirty state was not cleared");
        std::thread::yield_now();
    }
}

/// Repeated writes on a failing backed-off journal coalesce while a newly dirty
/// healthy journal still runs promptly.
#[test]
fn repeated_marks_preserve_failed_path_backoff() {
    let backend = Arc::new(TestBackend::default());
    let failed = PathBuf::from("/tmp/failing");
    {
        let mut state = backend.state.lock().expect("test backend lock");
        state.failed_files.insert(failed.clone());
        state.blocked_file = Some(failed.clone());
    }
    let worker = JournalSyncWorker::with_backend(backend.clone());
    worker.shared.state.lock().expect("worker state").retry_base = Duration::from_secs(60);
    worker.mark_dirty(&failed, 1, std::iter::empty());
    backend.wait_for(|state| state.blocked_entered);
    {
        let mut state = backend.state.lock().expect("test backend lock");
        state.release_block = true;
        backend.wake.notify_all();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !worker
        .shared
        .state
        .lock()
        .expect("worker state")
        .dirty
        .get(&failed)
        .is_some_and(|dirty| dirty.retry_at.is_some() && dirty.failures == 1)
    {
        assert!(
            Instant::now() < deadline,
            "failure backoff was not recorded"
        );
        std::thread::yield_now();
    }
    let (retry_at, failures) = {
        let state = worker.shared.state.lock().expect("worker state");
        let target = state.dirty.get(&failed).expect("failed target");
        (target.retry_at, target.failures)
    };
    for offset in 2..100 {
        worker.mark_dirty(&failed, offset, std::iter::empty());
    }
    {
        let state = worker.shared.state.lock().expect("worker state");
        let target = state.dirty.get(&failed).expect("failed target");
        assert_eq!(target.retry_at, retry_at);
        assert_eq!(target.failures, failures);
        assert_eq!(target.end_offset, 99);
    }
    let healthy = PathBuf::from("/tmp/healthy");
    worker.mark_dirty(&healthy, 1, std::iter::empty());
    backend.wait_for(|state| state.calls.contains(&('f', healthy.clone())));
    assert_eq!(backend.calls(), vec![('f', failed), ('f', healthy)]);
}

/// Relative paths sync their child directory before the normalized `.` parent.
#[test]
fn relative_directory_coverage_is_child_to_parent() {
    let backend = TestBackend::default();
    let target = DirtyJournal {
        kind: SyncTargetKind::DirectoryBoundary,
        generation: 1,
        end_offset: 0,
        directories: [PathBuf::from("state"), PathBuf::from(".")]
            .into_iter()
            .collect(),
        retry_at: None,
        failures: 0,
    };
    sync_target(&backend, Path::new("state/child"), &target).expect("sync target");
    assert_eq!(
        backend.calls(),
        vec![
            ('d', PathBuf::from("state/child")),
            ('d', PathBuf::from("state")),
            ('d', PathBuf::from(".")),
        ]
    );
}

/// A boundary child failure prevents false coverage of either ancestor.
#[test]
fn boundary_child_failure_stops_parent_sync() {
    let backend = TestBackend::default();
    backend
        .state
        .lock()
        .expect("test backend lock")
        .failed_directories
        .insert(PathBuf::from("state/child"));
    let target = DirtyJournal {
        kind: SyncTargetKind::DirectoryBoundary,
        generation: 1,
        end_offset: 0,
        directories: [PathBuf::from("state"), PathBuf::from(".")]
            .into_iter()
            .collect(),
        retry_at: None,
        failures: 0,
    };
    sync_target(&backend, Path::new("state/child"), &target).expect_err("child sync fails");
    assert_eq!(backend.calls(), vec![('d', PathBuf::from("state/child"))]);
}

//! Coalesced background writeback for semantic journals.

use std::sync as path_std_sync;
use std::sync::atomic as path_std_sync_atomic;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use std::{fmt, io, thread};

/// Filesystem operations used by one background sync attempt.
trait SyncBackend: Send + Sync + 'static {
    /// Syncs journal data through the current dirty watermark.
    fn sync_file(&self, path: &Path) -> io::Result<()>;

    /// Syncs one directory after its child journal or directory is covered.
    fn sync_directory(&self, path: &Path) -> io::Result<()>;
}

/// Control handle for a deterministic backend that blocks file sync.
#[cfg(test)]
pub(crate) struct BlockingSyncHandle {
    /// Shared entered/released state and wakeup.
    state: Arc<(Mutex<BlockingSyncState>, Condvar)>,
}

#[cfg(test)]
/// Backend that blocks journal file sync until its handle releases it.
struct BlockingSyncBackend {
    /// Shared entered/released state and wakeup.
    state: Arc<(Mutex<BlockingSyncState>, Condvar)>,
}

#[cfg(test)]
#[derive(Default)]
/// Named state for the deterministic blocking backend.
struct BlockingSyncState {
    /// Worker entered file sync.
    entered: bool,
    /// Test released the worker.
    released: bool,
}

#[cfg(test)]
impl SyncBackend for BlockingSyncBackend {
    fn sync_file(&self, _path: &Path) -> io::Result<()> {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().expect("blocking sync state");
        state.entered = true;
        wake.notify_all();
        while !state.released {
            state = wake.wait(state).expect("blocking sync wait");
        }
        Ok(())
    }

    fn sync_directory(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl BlockingSyncHandle {
    /// Waits up to two seconds for file sync and returns whether it entered.
    pub(crate) fn wait_until_blocked(&self) -> bool {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().expect("blocking sync state");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !state.entered {
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                return false;
            }
            (state, _) = wake
                .wait_timeout(state, timeout)
                .expect("blocking sync wait");
        }
        true
    }

    /// Releases the blocked worker.
    pub(crate) fn release(&self) {
        let (lock, wake) = &*self.state;
        lock.lock().expect("blocking sync state").released = true;
        wake.notify_all();
    }
}

/// Production filesystem sync backend.
struct FilesystemSync;

impl SyncBackend for FilesystemSync {
    fn sync_file(&self, path: &Path) -> io::Result<()> {
        File::open(path)?.sync_data()
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        File::open(path)?.sync_all()
    }
}

/// Lifecycle-owned worker that coalesces dirty state by journal or directory
/// boundary path.
pub(crate) struct JournalSyncWorker {
    /// Shared typed dirty-target state machine observed by owner and worker.
    shared: Arc<Shared>,
    /// Lazily spawned thread handle; dropping it deliberately detaches.
    thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// Filesystem or deterministic test implementation.
    backend: Arc<dyn SyncBackend>,
    /// Permanent best-effort degradation after thread creation fails.
    unavailable: path_std_sync::atomic::AtomicBool,
    #[cfg(test)]
    /// Injects one deterministic thread-spawn failure.
    fail_spawn: path_std_sync::atomic::AtomicBool,
    #[cfg(test)]
    /// Records that lazy startup reached the spawn attempt.
    spawn_attempted: path_std_sync::atomic::AtomicBool,
}

/// Filesystem object and ordering required by one dirty target.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SyncTargetKind {
    /// Sync journal data before its namespace directories.
    #[default]
    Journal,
    /// Sync a directory before the parent containing its entry.
    DirectoryBoundary,
}

/// Named dirty-target view exposed only to deterministic store tests.
#[cfg(test)]
pub(crate) struct DirtyTargetSnapshot {
    /// Required journal EOF; zero and unused for directory-boundary targets.
    pub(crate) end_offset: u64,
    /// Required ancestor directories; a boundary's primary path is separate.
    pub(crate) directories: BTreeSet<PathBuf>,
    /// Filesystem operation kind for the primary path.
    pub(crate) kind: SyncTargetKind,
}

impl fmt::Debug for JournalSyncWorker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JournalSyncWorker")
            .field(
                "running",
                &self
                    .thread
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl Default for JournalSyncWorker {
    fn default() -> Self {
        Self::with_backend(Arc::new(FilesystemSync))
    }
}

impl JournalSyncWorker {
    /// Builds a worker and control handle for store-level nonblocking tests.
    #[cfg(test)]
    pub(crate) fn blocking_for_test() -> (Self, BlockingSyncHandle) {
        let state = Arc::new((Mutex::new(BlockingSyncState::default()), Condvar::new()));
        (
            Self::with_backend(Arc::new(BlockingSyncBackend {
                state: Arc::clone(&state),
            })),
            BlockingSyncHandle { state },
        )
    }
    fn with_backend(backend: Arc<dyn SyncBackend>) -> Self {
        Self {
            shared: Arc::new(Shared::default()),
            thread: Mutex::new(None),
            backend,
            unavailable: path_std_sync_atomic::AtomicBool::new(false),
            #[cfg(test)]
            fail_spawn: path_std_sync_atomic::AtomicBool::new(false),
            #[cfg(test)]
            spawn_attempted: path_std_sync_atomic::AtomicBool::new(false),
        }
    }

    /// Marks a complete frame and exact directory-coverage debt dirty without
    /// waiting for writeback.
    pub(crate) fn mark_dirty(
        &self,
        path: &Path,
        end_offset: u64,
        directories: impl IntoIterator<Item = PathBuf>,
    ) {
        self.mark_target(path, end_offset, directories, SyncTargetKind::Journal);
    }

    /// Marks one directory boundary dirty without treating it as file data.
    pub(crate) fn mark_directory_boundary(
        &self,
        path: &Path,
        directories: impl IntoIterator<Item = PathBuf>,
    ) {
        self.mark_target(path, 0, directories, SyncTargetKind::DirectoryBoundary);
    }

    /// Merges one typed target and wakes the lazy worker.
    fn mark_target(
        &self,
        path: &Path,
        end_offset: u64,
        directories: impl IntoIterator<Item = PathBuf>,
        kind: SyncTargetKind,
    ) {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.next_generation = state.next_generation.wrapping_add(1);
            let generation = state.next_generation;
            let path = path.to_path_buf();
            let is_new = !state.dirty.contains_key(&path);
            let entry = state.dirty.entry(path.clone()).or_default();
            // ast-grep-ignore: debug-assert-expression-must-not-mutate
            debug_assert!(entry.generation == 0 || entry.kind == kind);
            entry.kind = kind;
            entry.generation = generation;
            entry.end_offset = entry.end_offset.max(end_offset);
            entry.directories.extend(directories);
            if is_new {
                state.ready.push_back(path);
            }
        }
        self.ensure_running();
        self.shared.wake.notify_one();
    }

    fn ensure_running(&self) {
        if self
            .unavailable
            .load(path_std_sync_atomic::Ordering::Relaxed)
        {
            return;
        }
        let mut worker = self
            .thread
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if worker.is_some() {
            return;
        }
        #[cfg(test)]
        self.spawn_attempted
            .store(true, path_std_sync_atomic::Ordering::Relaxed);
        #[cfg(test)]
        if self
            .fail_spawn
            .load(path_std_sync_atomic::Ordering::Relaxed)
        {
            eprintln!("tau: journal sync worker unavailable: injected spawn failure");
            self.unavailable
                .store(true, path_std_sync_atomic::Ordering::Relaxed);
            return;
        }
        let shared = Arc::clone(&self.shared);
        let backend = Arc::clone(&self.backend);
        match thread::Builder::new()
            .name("tau-journal-sync".to_owned())
            .spawn(move || shared.run(backend))
        {
            Ok(handle) => *worker = Some(handle),
            Err(error) => {
                eprintln!("tau: journal sync worker unavailable; relying on OS writeback: {error}");
                self.unavailable
                    .store(true, path_std_sync_atomic::Ordering::Relaxed);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn inject_spawn_failure(&self) {
        self.fail_spawn
            .store(true, path_std_sync_atomic::Ordering::Relaxed);
    }

    /// Returns one retained dirty target for deterministic store tests.
    #[cfg(test)]
    pub(crate) fn dirty_target(&self, path: &Path) -> Option<DirtyTargetSnapshot> {
        self.shared
            .state
            .lock()
            .expect("journal sync state")
            .dirty
            .get(path)
            .map(|target| DirtyTargetSnapshot {
                end_offset: target.end_offset,
                directories: target.directories.clone(),
                kind: target.kind,
            })
    }
}

impl Drop for JournalSyncWorker {
    fn drop(&mut self) {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.stopping = true;
        }
        self.shared.wake.notify_one();
        // A filesystem sync can block indefinitely. Detach rather than make store
        // destruction or the event loop wait for the worker.
        let _ = self
            .thread
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

#[derive(Default)]
/// Shared synchronization state owned by the store and detached worker.
struct Shared {
    /// Dirty targets and fair ready-path rotation.
    state: Mutex<State>,
    /// Wake for new work, retry-deadline changes, and shutdown.
    wake: Condvar,
}

/// Dirty-target map and fair ready queue protected by one mutex.
struct State {
    /// At most one merged dirty target per journal or directory boundary path.
    dirty: BTreeMap<PathBuf, DirtyJournal>,
    /// At most one ready entry per dirty target; in-flight paths are
    /// temporarily absent and requeued after their generation handshake.
    ready: VecDeque<PathBuf>,
    /// Wrapping generation used with the complete target as a watermark.
    next_generation: u64,
    /// Base delay for per-target exponential retry.
    retry_base: Duration,
    /// Owner requested one best-effort pass and detached.
    stopping: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            dirty: BTreeMap::new(),
            ready: VecDeque::new(),
            next_generation: 0,
            retry_base: Duration::from_millis(250),
            stopping: false,
        }
    }
}

#[derive(Clone, Default)]
/// Latest required writeback watermark for one journal or boundary path.
struct DirtyJournal {
    /// Whether this target syncs journal data or a directory boundary.
    kind: SyncTargetKind,
    /// Latest foreground dirty generation.
    generation: u64,
    /// Largest journal EOF requiring coverage; unused for a boundary target.
    end_offset: u64,
    /// Ancestor directories after the primary journal or boundary path.
    directories: BTreeSet<PathBuf>,
    /// Earliest retry time after a failed sync.
    retry_at: Option<Instant>,
    /// Consecutive failures used for capped exponential backoff.
    failures: u32,
}

impl Shared {
    /// Runs the detached writeback state machine until its owner stops it.
    fn run(self: Arc<Self>, backend: Arc<dyn SyncBackend>) {
        loop {
            let (path, target, stopping) = {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                let path = loop {
                    if state.stopping && state.ready.is_empty() {
                        return;
                    }
                    let now = Instant::now();
                    if let Some(path) = state.take_ready_path(now) {
                        break path;
                    }
                    let retry_wait = state
                        .dirty
                        .values()
                        .filter_map(|dirty| dirty.retry_at)
                        .min()
                        .map_or(Duration::from_secs(60), |deadline| {
                            deadline.saturating_duration_since(now)
                        });
                    let (next_state, _) = self
                        .wake
                        .wait_timeout(state, retry_wait)
                        .unwrap_or_else(|error| error.into_inner());
                    state = next_state;
                };
                let stopping = state.stopping;
                let target = state
                    .dirty
                    .get(&path)
                    .expect("ready path has dirty state")
                    .clone();
                (path, target, stopping)
            };

            let result = sync_target(backend.as_ref(), &path, &target);
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let target_covered = result.is_ok()
                && state.dirty.get(&path).is_some_and(|current| {
                    current.generation == target.generation
                        && current.kind == target.kind
                        && current.end_offset == target.end_offset
                        && current.directories == target.directories
                });
            if target_covered || stopping {
                state.dirty.remove(&path);
            } else {
                let retry_base = state.retry_base;
                if result.is_err()
                    && let Some(current) = state.dirty.get_mut(&path)
                {
                    current.failures = current.failures.saturating_add(1);
                    let shift = current.failures.saturating_sub(1).min(5);
                    current.retry_at =
                        Some(Instant::now() + retry_base.saturating_mul(1_u32 << shift));
                }
                state.ready.push_back(path.clone());
            }
            if let Err(error) = result {
                eprintln!(
                    "tau: background journal sync failed for {} through byte {}: {error}",
                    path.display(),
                    target.end_offset
                );
            }
        }
    }
}

impl State {
    /// Rotates fairly until it finds a path whose retry deadline has elapsed.
    fn take_ready_path(&mut self, now: Instant) -> Option<PathBuf> {
        let candidates = self.ready.len();
        for _ in 0..candidates {
            let path = self.ready.pop_front()?;
            let ready = self
                .dirty
                .get(&path)
                .is_some_and(|dirty| dirty.retry_at.is_none_or(|deadline| deadline <= now));
            if ready || self.stopping {
                return Some(path);
            }
            self.ready.push_back(path);
        }
        None
    }
}

fn sync_target(backend: &dyn SyncBackend, path: &Path, target: &DirtyJournal) -> io::Result<()> {
    match target.kind {
        SyncTargetKind::Journal => backend.sync_file(path)?,
        SyncTargetKind::DirectoryBoundary => backend.sync_directory(path)?,
    }
    let mut directories: Vec<_> = target.directories.iter().collect();
    directories.sort_by(|left, right| {
        directory_depth(right)
            .cmp(&directory_depth(left))
            .then_with(|| left.cmp(right))
    });
    for directory in directories {
        backend.sync_directory(directory)?;
    }
    Ok(())
}

/// Returns a normalized ancestry depth where `.` is the relative root.
fn directory_depth(path: &Path) -> usize {
    if path == Path::new(".") {
        0
    } else {
        path.components().count()
    }
}

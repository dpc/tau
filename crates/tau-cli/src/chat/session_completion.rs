//! Demand-driven running-session completion for one interactive attachment.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tau_cli_term::{ArgCompleter, CompletionItem, TermHandle};
use tau_harness::runtime_dir::RunningSessionSnapshot;
use tau_proto::SessionId;

const FRESH_TTL: Duration = Duration::from_secs(5);
const FAILURE_BACKOFF: Duration = Duration::from_secs(2);
const STALE_TTL: Duration = Duration::from_secs(30);

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;
type Discovery = Arc<dyn Fn() -> io::Result<RunningSessionSnapshot> + Send + Sync>;

/// Owns one lazy, single-flight discovery worker for an interactive attachment.
pub(super) struct SessionCompletion {
    /// Shared worker control retained until attachment cleanup.
    control: Arc<WorkerControl>,
}

impl SessionCompletion {
    /// Creates session completion backed by tolerant running-session discovery.
    pub(super) fn new(current_session: SessionId, term: TermHandle) -> Self {
        Self::new_with(
            current_session,
            term,
            Arc::new(Instant::now),
            Arc::new(tau_harness::runtime_dir::list_running_sessions_tolerant),
        )
    }

    /// Returns the synchronous completion callback installed in terminal data.
    pub(super) fn completer(&self) -> ArgCompleter {
        let control = Arc::downgrade(&self.control);
        Arc::new(move |args: &[&str]| {
            let [partial] = args else {
                return Vec::new();
            };
            let Some(control) = control.upgrade() else {
                return Vec::new();
            };
            control.complete(partial)
        })
    }

    /// Creates completion with injected clock and discovery seams for tests.
    fn new_with(
        current_session: SessionId,
        term: TermHandle,
        clock: Clock,
        discovery: Discovery,
    ) -> Self {
        Self {
            control: Arc::new(WorkerControl {
                current_session,
                term,
                clock,
                discovery,
                cache: Arc::new(Mutex::new(Cache::default())),
                worker: Mutex::new(None),
                latest_generation: Arc::new(AtomicU64::new(0)),
                shutdown: Arc::new(AtomicBool::new(false)),
            }),
        }
    }
}

impl Drop for SessionCompletion {
    fn drop(&mut self) {
        self.control.shutdown();
    }
}

/// Mutable cache and lazy worker ownership retained by the attachment owner.
struct WorkerControl {
    /// Session receiving the `current` menu marker.
    current_session: SessionId,
    /// Terminal wake handle used only after a discovery result arrives.
    term: TermHandle,
    /// Injected monotonic clock.
    clock: Clock,
    /// Injected tolerant discovery operation.
    discovery: Discovery,
    /// Last successful snapshot and failure timing.
    cache: Arc<Mutex<Cache>>,
    /// Lazily created worker and coalescing demand sender.
    worker: Mutex<Option<Worker>>,
    /// Most recent completion interaction interested in an in-flight scan.
    latest_generation: Arc<AtomicU64>,
    /// Stops late wakeups and asks the worker to exit.
    shutdown: Arc<AtomicBool>,
}

impl WorkerControl {
    /// Returns cached matches immediately and requests discovery when stale.
    fn complete(&self, partial: &str) -> Vec<CompletionItem> {
        let generation = self.term.completion_refresh_generation();
        self.latest_generation.store(generation, Ordering::Release);
        let now = (self.clock)();
        let (items, refresh) = self
            .cache
            .lock()
            .expect("session completion cache")
            .read(now);
        if refresh {
            self.request_discovery();
        }
        let needle = partial.to_lowercase();
        let mut prefix_matches = Vec::new();
        let mut substring_matches = Vec::new();
        for indexed in items.iter() {
            if needle.is_empty() || indexed.lower_value.starts_with(&needle) {
                prefix_matches.push(indexed.item.clone());
            } else if indexed.lower_value.contains(&needle) {
                substring_matches.push(indexed.item.clone());
            }
        }
        prefix_matches.extend(substring_matches);
        prefix_matches
    }

    /// Starts the worker lazily and coalesces demand into its single slot.
    fn request_discovery(&self) {
        let mut worker = self.worker.lock().expect("session completion worker");
        if worker.is_none() {
            *worker = Some(Worker::spawn(
                self.current_session.clone(),
                self.term.clone(),
                Arc::clone(&self.clock),
                Arc::clone(&self.discovery),
                Arc::clone(&self.cache),
                Arc::clone(&self.latest_generation),
                Arc::clone(&self.shutdown),
            ));
        }
        if let Some(worker) = worker.as_ref() {
            let _ = worker.demand.try_send(());
        }
    }

    /// Stops and joins the worker after its bounded current discovery
    /// completes.
    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let worker = self
            .worker
            .lock()
            .expect("session completion worker")
            .take();
        if let Some(worker) = worker {
            let _ = worker.demand.try_send(());
            let _ = worker.join.join();
        }
    }
}

/// Single worker thread and its one-slot coalescing demand channel.
struct Worker {
    /// Nonblocking demand sender used by the synchronous completion path.
    demand: mpsc::SyncSender<()>,
    /// Worker join handle retained for attachment cleanup.
    join: JoinHandle<()>,
}

impl Worker {
    /// Spawns one idle-until-demand discovery worker.
    fn spawn(
        current_session: SessionId,
        term: TermHandle,
        clock: Clock,
        discovery: Discovery,
        cache: Arc<Mutex<Cache>>,
        latest_generation: Arc<AtomicU64>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        let (demand, demand_rx) = mpsc::sync_channel(1);
        let join = std::thread::spawn(move || {
            while demand_rx.recv().is_ok() {
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                let result = discovery();
                let observed_at = clock();
                {
                    let mut cache = cache.lock().expect("session completion cache");
                    match result {
                        Ok(snapshot) => {
                            if 0 < snapshot.incomplete_claims {
                                tracing::debug!(
                                    incomplete_claims = snapshot.incomplete_claims,
                                    "session completion discovery omitted incomplete claims"
                                );
                            }
                            cache.record_success(
                                observed_at,
                                completion_items(snapshot, &current_session),
                            );
                        }
                        Err(error) => {
                            cache.record_failure(observed_at);
                            tracing::debug!(
                                %error,
                                "session completion discovery failed"
                            );
                        }
                    }
                }
                if shutdown.load(Ordering::Acquire) {
                    break;
                }
                while demand_rx.try_recv().is_ok() {}
                let generation = latest_generation.load(Ordering::Acquire);
                term.request_completion_refresh_if_generation(generation);
            }
        });
        Self { demand, join }
    }
}

/// Last good immutable candidate snapshot and retry timing.
#[derive(Default)]
struct Cache {
    /// Candidate rows from the last successful discovery.
    items: Arc<Vec<IndexedCompletion>>,
    /// Monotonic time of the last successful discovery.
    last_success: Option<Instant>,
    /// Monotonic time of the last top-level discovery failure.
    last_failure: Option<Instant>,
}

impl Cache {
    /// Returns the currently usable snapshot and whether demand should refresh
    /// it.
    fn read(&self, now: Instant) -> (Arc<Vec<IndexedCompletion>>, bool) {
        let fresh = self
            .last_success
            .is_some_and(|at| now.saturating_duration_since(at) <= FRESH_TTL);
        let backed_off = self
            .last_failure
            .is_some_and(|at| now.saturating_duration_since(at) < FAILURE_BACKOFF);
        let usable = self
            .last_success
            .is_some_and(|at| now.saturating_duration_since(at) <= STALE_TTL);
        (
            if usable {
                Arc::clone(&self.items)
            } else {
                Arc::new(Vec::new())
            },
            !fresh && !backed_off,
        )
    }

    /// Replaces the whole cache after one successful tolerant discovery.
    fn record_success(&mut self, observed_at: Instant, items: Vec<IndexedCompletion>) {
        self.items = Arc::new(items);
        self.last_success = Some(observed_at);
        self.last_failure = None;
    }

    /// Retains the last good snapshot and starts finite retry backoff.
    fn record_failure(&mut self, observed_at: Instant) {
        self.last_failure = Some(observed_at);
    }
}

/// One cached menu row with its precomputed case-insensitive matching key.
struct IndexedCompletion {
    /// Terminal-facing completion row.
    item: CompletionItem,
    /// Lowercase session identifier used for per-keystroke matching.
    lower_value: String,
}

/// Converts exact running sessions into deterministic terminal menu rows.
fn completion_items(
    snapshot: RunningSessionSnapshot,
    current_session: &SessionId,
) -> Vec<IndexedCompletion> {
    snapshot
        .sessions
        .into_iter()
        .map(|session| {
            let mut description = session.project_root.display().to_string();
            if &session.session_id == current_session {
                description.push_str(" (current)");
            }
            let value = session.session_id.to_string();
            IndexedCompletion {
                lower_value: value.to_lowercase(),
                item: CompletionItem::new(value, description),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;

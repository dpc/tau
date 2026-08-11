//! Bounded priority scheduler for ext-shell work.
//!
//! The protocol reader must never block on worker capacity: it enqueues owned
//! work here, and this scheduler runs it on a fixed set of native worker
//! threads. Bounded queues provide backpressure without spawning unbounded
//! waiter threads or rejecting ordinary short bursts while workers are busy.

#[cfg(test)]
mod tests;
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use tau_proto::{AgentId, ToolCallId};
use tracing::warn;

/// Default aggregate cap for approximated queued argument bytes.
pub(crate) const DEFAULT_QUEUED_BYTES_LIMIT: usize = 1024 * 1024;

/// Queue lane used to pick the next runnable ext-shell work item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkPriority {
    /// Control work that must not be starved by model tool bursts.
    Control,
    /// User-initiated shell work from `!` / `!!`.
    User,
    /// Read-only or otherwise cheap model-visible tools.
    Cheap,
    /// Mutating tools and model shell commands.
    Bulk,
}

/// Error returned when bounded scheduler queues cannot admit more work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnqueueError {
    /// Human-readable reason suitable for a tool error.
    pub(crate) message: String,
}

/// Metadata needed to cancel queued work before it starts.
pub(crate) struct WorkMeta {
    /// Tool call id, when this work corresponds to a model tool invocation.
    pub(crate) call_id: Option<ToolCallId>,
    /// Owner agent used for lifecycle cleanup.
    pub(crate) agent_id: Option<AgentId>,
    /// Approximate queued argument byte cost for bounded memory accounting.
    pub(crate) queued_bytes: usize,
}

struct WorkItem {
    meta: WorkMeta,
    job: Box<dyn FnOnce() + Send + 'static>,
}

/// Tunable hard bounds for scheduler queues and workers.
#[derive(Clone, Debug)]
pub(crate) struct SchedulerConfig {
    /// Maximum number of queued items across all lanes.
    pub(crate) total_limit: usize,
    /// Maximum queued control-lane items such as non-update `dir_lock` calls.
    pub(crate) control_limit: usize,
    /// Maximum queued user-shell items from `!` / `!!`.
    pub(crate) user_limit: usize,
    /// Maximum queued cheap/read-only model-visible tool items.
    pub(crate) cheap_limit: usize,
    /// Maximum queued bulk model work, including mutating tools and model
    /// shells.
    pub(crate) bulk_limit: usize,
    /// Approximate aggregate byte budget for queued call arguments.
    pub(crate) queued_bytes_limit: usize,
    /// Dedicated workers that only run control-lane work.
    pub(crate) control_workers: usize,
    /// Dedicated workers that run control or user-shell work.
    pub(crate) user_workers: usize,
    /// Dedicated workers that run control, user, or cheap/read-only work.
    pub(crate) cheap_workers: usize,
    /// General workers that can run any scheduler lane, including bulk work.
    pub(crate) general_workers: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            total_limit: 64,
            control_limit: 16,
            user_limit: 16,
            cheap_limit: 32,
            bulk_limit: 32,
            queued_bytes_limit: DEFAULT_QUEUED_BYTES_LIMIT,
            control_workers: 1,
            user_workers: 2,
            cheap_workers: 3,
            general_workers: 10,
        }
    }
}

/// Fixed-worker bounded priority scheduler for ext-shell work.
///
/// Dropping the scheduler is a shutdown boundary: queued work is discarded,
/// workers are woken, and already-running worker jobs are joined before `Drop`
/// returns. Callers that own blocking resources used by worker jobs must cancel
/// those resources before dropping the scheduler.
pub(crate) struct WorkScheduler {
    inner: Arc<Inner>,
    worker_handles: Vec<JoinHandle<()>>,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
    config: SchedulerConfig,
}

#[derive(Default)]
struct State {
    control: VecDeque<WorkItem>,
    user: VecDeque<WorkItem>,
    cheap: VecDeque<WorkItem>,
    bulk: VecDeque<WorkItem>,
    queued_bytes: usize,
    shutdown: bool,
}

#[derive(Clone, Copy)]
enum WorkerKind {
    Control,
    User,
    Cheap,
    General,
}

impl WorkScheduler {
    /// Create a scheduler and spawn its bounded worker set.
    pub(crate) fn new(config: SchedulerConfig) -> Self {
        let mut scheduler = Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
                config,
            }),
            worker_handles: Vec::new(),
        };
        scheduler.spawn_workers();
        scheduler
    }

    /// Return the configured aggregate queued argument byte budget.
    pub(crate) fn queued_bytes_limit(&self) -> usize {
        self.inner.config.queued_bytes_limit
    }

    /// Enqueue work or return a bounded-backpressure error.
    pub(crate) fn enqueue(
        &self,
        priority: WorkPriority,
        meta: WorkMeta,
        job: impl FnOnce() + Send + 'static,
    ) -> Result<(), EnqueueError> {
        let mut state = self.inner.state.lock().expect("scheduler state poisoned");
        let config = &self.inner.config;
        let lane_len = state.lane(priority).len();
        let lane_limit = match priority {
            WorkPriority::Control => config.control_limit,
            WorkPriority::User => config.user_limit,
            WorkPriority::Cheap => config.cheap_limit,
            WorkPriority::Bulk => config.bulk_limit,
        };
        if config.total_limit <= state.total_len() {
            return Err(EnqueueError {
                message: format!(
                    "too many queued shell tool calls; queue limit is {}",
                    config.total_limit
                ),
            });
        }
        if lane_limit <= lane_len {
            return Err(EnqueueError {
                message: format!(
                    "too many queued {:?} shell tool calls; queue limit is {lane_limit}",
                    priority
                ),
            });
        }
        if config.queued_bytes_limit < state.queued_bytes.saturating_add(meta.queued_bytes) {
            return Err(EnqueueError {
                message: format!(
                    "queued shell tool arguments exceed {} byte limit",
                    config.queued_bytes_limit
                ),
            });
        }

        state.queued_bytes = state.queued_bytes.saturating_add(meta.queued_bytes);
        state.lane_mut(priority).push_back(WorkItem {
            meta,
            job: Box::new(job),
        });
        self.inner.changed.notify_all();
        Ok(())
    }

    /// Cancel queued work before it starts, returning true if a queued call was
    /// removed.
    pub(crate) fn cancel_queued_call(&self, call_id: &ToolCallId) -> bool {
        let mut state = self.inner.state.lock().expect("scheduler state poisoned");
        let Some(item) = state.remove_call(call_id) else {
            return false;
        };
        drop(state);
        drop(item);
        true
    }

    /// Remove queued work for an unloaded/finished agent.
    pub(crate) fn cancel_agent(&self, agent_id: &AgentId) -> usize {
        let mut state = self.inner.state.lock().expect("scheduler state poisoned");
        state.remove_agent(agent_id)
    }

    /// Remove all queued work for session shutdown/disconnect cleanup.
    pub(crate) fn cancel_all_queued(&self) -> usize {
        let mut state = self.inner.state.lock().expect("scheduler state poisoned");
        let removed = state.total_len();
        state.clear_queues();
        removed
    }

    fn spawn_workers(&mut self) {
        let config = self.inner.config.clone();
        for _ in 0..config.control_workers {
            self.spawn_worker(WorkerKind::Control);
        }
        for _ in 0..config.user_workers {
            self.spawn_worker(WorkerKind::User);
        }
        for _ in 0..config.cheap_workers {
            self.spawn_worker(WorkerKind::Cheap);
        }
        for _ in 0..config.general_workers {
            self.spawn_worker(WorkerKind::General);
        }
    }

    fn spawn_worker(&mut self, kind: WorkerKind) {
        let inner = Arc::clone(&self.inner);
        let handle = std::thread::spawn(move || worker_loop(inner, kind));
        self.worker_handles.push(handle);
    }
}

impl Drop for WorkScheduler {
    fn drop(&mut self) {
        {
            let mut state = self.inner.state.lock().expect("scheduler state poisoned");
            state.shutdown = true;
            state.clear_queues();
            self.inner.changed.notify_all();
        }
        let handles = std::mem::take(&mut self.worker_handles);
        for handle in handles {
            if handle.join().is_err() {
                warn!("scheduler worker panicked during shutdown");
            }
        }
    }
}

impl State {
    fn total_len(&self) -> usize {
        self.control.len() + self.user.len() + self.cheap.len() + self.bulk.len()
    }

    fn lane(&self, priority: WorkPriority) -> &VecDeque<WorkItem> {
        match priority {
            WorkPriority::Control => &self.control,
            WorkPriority::User => &self.user,
            WorkPriority::Cheap => &self.cheap,
            WorkPriority::Bulk => &self.bulk,
        }
    }

    fn lane_mut(&mut self, priority: WorkPriority) -> &mut VecDeque<WorkItem> {
        match priority {
            WorkPriority::Control => &mut self.control,
            WorkPriority::User => &mut self.user,
            WorkPriority::Cheap => &mut self.cheap,
            WorkPriority::Bulk => &mut self.bulk,
        }
    }

    fn pop_for(&mut self, kind: WorkerKind) -> Option<WorkItem> {
        match kind {
            WorkerKind::Control => self.pop_priority(&[WorkPriority::Control]),
            WorkerKind::User => self.pop_priority(&[WorkPriority::Control, WorkPriority::User]),
            WorkerKind::Cheap => self.pop_priority(&[
                WorkPriority::Control,
                WorkPriority::User,
                WorkPriority::Cheap,
            ]),
            WorkerKind::General => self.pop_priority(&[
                WorkPriority::Control,
                WorkPriority::User,
                WorkPriority::Cheap,
                WorkPriority::Bulk,
            ]),
        }
    }

    fn pop_priority(&mut self, priorities: &[WorkPriority]) -> Option<WorkItem> {
        for priority in priorities {
            if let Some(item) = self.lane_mut(*priority).pop_front() {
                self.queued_bytes = self.queued_bytes.saturating_sub(item.meta.queued_bytes);
                return Some(item);
            }
        }
        None
    }

    fn remove_call(&mut self, call_id: &ToolCallId) -> Option<WorkItem> {
        for priority in [
            WorkPriority::Control,
            WorkPriority::User,
            WorkPriority::Cheap,
            WorkPriority::Bulk,
        ] {
            let lane = self.lane_mut(priority);
            if let Some(pos) = lane
                .iter()
                .position(|item| item.meta.call_id.as_ref() == Some(call_id))
            {
                let item = lane.remove(pos).expect("position exists");
                self.queued_bytes = self.queued_bytes.saturating_sub(item.meta.queued_bytes);
                return Some(item);
            }
        }
        None
    }

    fn remove_agent(&mut self, agent_id: &AgentId) -> usize {
        let mut removed = 0usize;
        let mut removed_bytes = 0usize;
        for priority in [
            WorkPriority::Control,
            WorkPriority::User,
            WorkPriority::Cheap,
            WorkPriority::Bulk,
        ] {
            let lane = self.lane_mut(priority);
            let mut kept = VecDeque::new();
            while let Some(item) = lane.pop_front() {
                if item.meta.agent_id.as_ref() == Some(agent_id) {
                    removed_bytes = removed_bytes.saturating_add(item.meta.queued_bytes);
                    removed += 1;
                } else {
                    kept.push_back(item);
                }
            }
            *lane = kept;
        }
        self.queued_bytes = self.queued_bytes.saturating_sub(removed_bytes);
        removed
    }

    fn clear_queues(&mut self) {
        self.control.clear();
        self.user.clear();
        self.cheap.clear();
        self.bulk.clear();
        self.queued_bytes = 0;
    }
}

fn worker_loop(inner: Arc<Inner>, kind: WorkerKind) {
    loop {
        let item = {
            let mut state = inner.state.lock().expect("scheduler state poisoned");
            loop {
                if state.shutdown {
                    return;
                }
                if let Some(item) = state.pop_for(kind) {
                    break item;
                }
                state = inner.changed.wait(state).expect("scheduler state poisoned");
            }
        };
        (item.job)();
    }
}

//! Directory update lock manager for shell-owned mutating tools.
//!
//! The lock is advisory and lives inside `tau-ext-shell`: reads never wait,
//! while shell/file update tools coordinate on canonical absolute directory
//! paths. Manual `dir_lock update` calls reserve a subtree for their owning
//! agent, and automatic writer locks serialize concrete mutating operations.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use tau_proto::{
    AgentId, CborValue, Event, HarnessInputMessage, ToolCallId, ToolCancelled, ToolError,
    ToolProgress, ToolResult, ToolResultKind, ToolStarted, ToolType, ToolUseState, ToolUseStatus,
};

use crate::argument::{argument_text, optional_argument_text};
use crate::display::{ToolFailure, ok_display};
use crate::tools::{APPLY_PATCH_TOOL_NAME, EDIT_TOOL_NAME, GPT_SHELL_TOOL_NAME, SHELL_TOOL_NAME};

/// Agent-facing name of the directory locking tool.
pub(crate) const DIR_LOCK_TOOL_NAME: &str = "dir_lock";

const DEFAULT_LOCK_WAIT_LIVENESS_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_LOCK_ABANDONED_AFTER: Duration = Duration::from_secs(120);
const ABANDONED_LOCK_ERROR: &str = "dir_lock_abandoned";
const ABANDONED_LOCK_OUTPUT: &str = "Directory locked and inactive - possibly abandoned. Consider messaging the lock owner agent and/or force-unlocking with `dir_lock unlock` using `blocking_directory` as `directory` and `lock_owner_id` as `owner_agent_id`.";
const DUPLICATE_LOCK_ERROR: &str = "dir_lock_duplicate";
const DUPLICATE_LOCK_OUTPUT: &str = "Directory lock already held by this agent. Unlock the existing lock before locking another overlapping directory.";

#[derive(Clone, Copy, Debug)]
struct LockWaitPolicy {
    liveness_interval: Duration,
    abandoned_after: Duration,
}

impl Default for LockWaitPolicy {
    fn default() -> Self {
        Self {
            liveness_interval: DEFAULT_LOCK_WAIT_LIVENESS_INTERVAL,
            abandoned_after: DEFAULT_LOCK_ABANDONED_AFTER,
        }
    }
}

/// Manual lock state that appears stale to a waiting lock request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AbandonedLock {
    /// Agent that owns the blocking manual lock.
    pub(crate) owner: AgentId,
    /// Canonical manual lock directory blocking the request.
    pub(crate) dir: PathBuf,
    /// How long the lock has existed.
    pub(crate) held_for: Duration,
    /// How long it has been since the lock was acquired or used by an automatic
    /// tool.
    pub(crate) idle_for: Duration,
}

impl AbandonedLock {
    fn message(&self) -> String {
        ABANDONED_LOCK_ERROR.to_owned()
    }

    fn details(&self) -> CborValue {
        CborValue::Map(vec![
            cbor_text_entry("blocking_directory", self.dir.display().to_string()),
            cbor_text_entry("lock_owner_id", self.owner.to_string()),
            cbor_duration_seconds_entry("idle_seconds", self.idle_for),
            cbor_duration_seconds_entry("held_seconds", self.held_for),
            cbor_text_entry("output", ABANDONED_LOCK_OUTPUT),
        ])
    }

    pub(crate) fn tool_failure(&self) -> ToolFailure {
        ToolFailure::from(self.message())
            .with_args(self.dir.display().to_string())
            .with_details(self.details())
    }
}

fn duplicate_manual_lock_details(
    owner: &AgentId,
    blocking_dir: &Path,
    requested_dir: &Path,
) -> CborValue {
    CborValue::Map(vec![
        cbor_text_entry("blocking_directory", blocking_dir.display().to_string()),
        cbor_text_entry("requested_directory", requested_dir.display().to_string()),
        cbor_text_entry("lock_owner_id", owner.to_string()),
        cbor_text_entry("output", DUPLICATE_LOCK_OUTPUT),
    ])
}

fn cbor_text_entry(key: &str, value: impl Into<String>) -> (CborValue, CborValue) {
    (
        CborValue::Text(key.to_owned()),
        CborValue::Text(value.into()),
    )
}

fn cbor_duration_seconds_entry(key: &str, duration: Duration) -> (CborValue, CborValue) {
    let seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    (
        CborValue::Text(key.to_owned()),
        CborValue::Integer(seconds.into()),
    )
}

/// Shared state used by all ext-shell workers that participate in directory
/// update locking.
#[derive(Clone, Debug, Default)]
pub(crate) struct DirLockManager {
    inner: Arc<DirLockInner>,
}

#[derive(Debug, Default)]
struct DirLockInner {
    state: Mutex<LockState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct LockState {
    manual: Vec<ManualLock>,
    automatic: Vec<AutomaticLock>,
    waiters: VecDeque<Waiter>,
    next_waiter_id: u64,
    next_auto_id: u64,
}

#[derive(Clone, Debug)]
struct ManualLock {
    owner: AgentId,
    dir: PathBuf,
    acquired_at: Instant,
    last_used_at: Instant,
    active_auto_ids: Vec<u64>,
}

#[derive(Clone, Debug)]
struct AutomaticLock {
    id: u64,
    owner: AgentId,
    dirs: Vec<PathBuf>,
}

/// Manual directory lock removed by a user force-unlock action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ForceUnlockedLock {
    /// Agent that owned the manual lock.
    pub(crate) owner: AgentId,
    /// Canonical directory that was locked.
    pub(crate) dir: PathBuf,
}

#[derive(Clone, Debug)]
struct Waiter {
    id: u64,
    call_id: ToolCallId,
    owner: AgentId,
    dirs: Vec<PathBuf>,
    kind: WaitKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitKind {
    Manual,
    Automatic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LockAcquireError {
    Cancelled,
    Abandoned(AbandonedLock),
    SelfConflict { dir: PathBuf },
    NotCovered,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManualLockAcquireError {
    Cancelled,
    AlreadyHeld { dir: PathBuf },
    Abandoned(AbandonedLock),
}

/// RAII guard for an automatic writer lock. Dropping it releases the active
/// lock and wakes the next FIFO waiter.
#[derive(Debug)]
pub(crate) struct AutoDirLockGuard {
    manager: DirLockManager,
    id: u64,
}

impl Drop for AutoDirLockGuard {
    fn drop(&mut self) {
        self.manager.release_auto(self.id);
    }
}

impl DirLockManager {
    /// Acquire an automatic update lock for one mutating tool invocation.
    pub(crate) fn acquire_auto<F>(
        &self,
        call_id: ToolCallId,
        owner: AgentId,
        dirs: Vec<PathBuf>,
        on_wait: F,
    ) -> Result<AutoDirLockGuard, LockAcquireError>
    where
        F: FnOnce(),
    {
        self.acquire_auto_with_policy(call_id, owner, dirs, on_wait, LockWaitPolicy::default())
    }

    /// Acquire an automatic update lock only while `owner` still holds a manual
    /// lock covering every requested directory.
    pub(crate) fn acquire_auto_if_manual_covers<F>(
        &self,
        call_id: ToolCallId,
        owner: AgentId,
        dirs: Vec<PathBuf>,
        on_wait: F,
    ) -> Result<AutoDirLockGuard, LockAcquireError>
    where
        F: FnOnce(),
    {
        self.acquire_auto_with_policy_and_manual_requirement(
            call_id,
            owner,
            dirs,
            on_wait,
            LockWaitPolicy::default(),
            true,
        )
    }

    fn acquire_auto_with_policy<F>(
        &self,
        call_id: ToolCallId,
        owner: AgentId,
        dirs: Vec<PathBuf>,
        on_wait: F,
        policy: LockWaitPolicy,
    ) -> Result<AutoDirLockGuard, LockAcquireError>
    where
        F: FnOnce(),
    {
        self.acquire_auto_with_policy_and_manual_requirement(
            call_id, owner, dirs, on_wait, policy, false,
        )
    }

    fn acquire_auto_with_policy_and_manual_requirement<F>(
        &self,
        call_id: ToolCallId,
        owner: AgentId,
        dirs: Vec<PathBuf>,
        on_wait: F,
        policy: LockWaitPolicy,
        require_manual_cover: bool,
    ) -> Result<AutoDirLockGuard, LockAcquireError>
    where
        F: FnOnce(),
    {
        let dirs = normalize_lock_dirs(dirs);
        let mut on_wait = Some(on_wait);
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        if require_manual_cover && !state.manual_covers(&owner, &dirs) {
            return Err(LockAcquireError::NotCovered);
        }
        if !state.manual_covers(&owner, &dirs)
            && let Some(dir) = state.manual_lock_owned_overlapping(&owner, &dirs)
        {
            return Err(LockAcquireError::SelfConflict { dir });
        }
        if state.can_grant_now(&owner, &dirs, WaitKind::Automatic) {
            let id = state.add_auto(owner, dirs);
            return Ok(AutoDirLockGuard {
                manager: self.clone(),
                id,
            });
        }

        let waiter = state.push_waiter(call_id, owner, dirs, WaitKind::Automatic);
        drop(state);
        if let Some(on_wait) = on_wait.take() {
            on_wait();
        }
        let mut next_liveness_check = Instant::now() + policy.liveness_interval;
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        loop {
            let Some(pos) = state.waiters.iter().position(|queued| queued.id == waiter) else {
                return Err(LockAcquireError::Cancelled);
            };
            if pos == 0 {
                let queued = state.waiters.front().expect("position says front exists");
                if require_manual_cover && !state.manual_covers(&queued.owner, &queued.dirs) {
                    state.waiters.pop_front().expect("front exists");
                    self.inner.changed.notify_all();
                    return Err(LockAcquireError::NotCovered);
                }
                if !state.has_conflict(&queued.owner, &queued.dirs, queued.kind) {
                    let queued = state.waiters.pop_front().expect("front exists");
                    let id = state.add_auto(queued.owner, queued.dirs);
                    return Ok(AutoDirLockGuard {
                        manager: self.clone(),
                        id,
                    });
                }
                let now = Instant::now();
                if next_liveness_check <= now {
                    if let Some(blocker) = state.abandoned_blocker(
                        &queued.owner,
                        &queued.dirs,
                        queued.kind,
                        now,
                        policy.abandoned_after,
                    ) {
                        state.waiters.pop_front().expect("front exists");
                        self.inner.changed.notify_all();
                        return Err(LockAcquireError::Abandoned(blocker));
                    }
                    next_liveness_check = now + policy.liveness_interval;
                }
            }
            let now = Instant::now();
            if next_liveness_check <= now {
                next_liveness_check = now + policy.liveness_interval;
            }
            let wait_for = next_liveness_check.saturating_duration_since(now);
            let (new_state, _) = self
                .inner
                .changed
                .wait_timeout(state, wait_for)
                .expect("dir lock state poisoned");
            state = new_state;
        }
    }

    /// Acquire and retain a manual lock owned by `owner`.
    pub(crate) fn acquire_manual<F>(
        &self,
        call_id: ToolCallId,
        owner: AgentId,
        dir: PathBuf,
        on_wait: F,
    ) -> Result<(), ManualLockAcquireError>
    where
        F: FnOnce(),
    {
        self.acquire_manual_with_policy(call_id, owner, dir, on_wait, LockWaitPolicy::default())
    }

    fn acquire_manual_with_policy<F>(
        &self,
        call_id: ToolCallId,
        owner: AgentId,
        dir: PathBuf,
        on_wait: F,
        policy: LockWaitPolicy,
    ) -> Result<(), ManualLockAcquireError>
    where
        F: FnOnce(),
    {
        let dirs = vec![dir];
        let mut on_wait = Some(on_wait);
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        if let Some(held_dir) = state.manual_lock_owned_overlapping(&owner, &dirs) {
            return Err(ManualLockAcquireError::AlreadyHeld { dir: held_dir });
        }
        if state.can_grant_now(&owner, &dirs, WaitKind::Manual) {
            state.add_manual(owner, dirs, Instant::now());
            self.inner.changed.notify_all();
            return Ok(());
        }

        let waiter = state.push_waiter(call_id, owner, dirs, WaitKind::Manual);
        drop(state);
        if let Some(on_wait) = on_wait.take() {
            on_wait();
        }
        let mut next_liveness_check = Instant::now() + policy.liveness_interval;
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        loop {
            let Some(pos) = state.waiters.iter().position(|queued| queued.id == waiter) else {
                return Err(ManualLockAcquireError::Cancelled);
            };
            if pos == 0 {
                let queued = state.waiters.front().expect("position says front exists");
                if let Some(held_dir) =
                    state.manual_lock_owned_overlapping(&queued.owner, &queued.dirs)
                {
                    state.waiters.pop_front().expect("front exists");
                    self.inner.changed.notify_all();
                    return Err(ManualLockAcquireError::AlreadyHeld { dir: held_dir });
                }
                if !state.has_conflict(&queued.owner, &queued.dirs, queued.kind) {
                    let queued = state.waiters.pop_front().expect("front exists");
                    state.add_manual(queued.owner, queued.dirs, Instant::now());
                    self.inner.changed.notify_all();
                    return Ok(());
                }
                let now = Instant::now();
                if next_liveness_check <= now {
                    if let Some(blocker) = state.abandoned_blocker(
                        &queued.owner,
                        &queued.dirs,
                        queued.kind,
                        now,
                        policy.abandoned_after,
                    ) {
                        state.waiters.pop_front().expect("front exists");
                        self.inner.changed.notify_all();
                        return Err(ManualLockAcquireError::Abandoned(blocker));
                    }
                    next_liveness_check = now + policy.liveness_interval;
                }
            }
            let now = Instant::now();
            if next_liveness_check <= now {
                next_liveness_check = now + policy.liveness_interval;
            }
            let wait_for = next_liveness_check.saturating_duration_since(now);
            let (new_state, _) = self
                .inner
                .changed
                .wait_timeout(state, wait_for)
                .expect("dir lock state poisoned");
            state = new_state;
        }
    }

    /// Release one exact manual lock held by `owner` for `dir`.
    pub(crate) fn unlock_manual(&self, owner: &AgentId, dir: &Path) -> Result<(), String> {
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        let Some(pos) = state
            .manual
            .iter()
            .position(|lock| &lock.owner == owner && lock.dir == dir)
        else {
            return Err(format!(
                "agent `{owner}` does not hold a directory lock for {}",
                dir.display()
            ));
        };
        state.manual.remove(pos);
        self.inner.changed.notify_all();
        Ok(())
    }

    /// Cancel a queued lock waiter for `call_id`, if one exists.
    pub(crate) fn cancel_waiting_call(&self, call_id: &ToolCallId) -> bool {
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        let before = state.waiters.len();
        state.waiters.retain(|waiter| &waiter.call_id != call_id);
        let removed = state.waiters.len() != before;
        if removed {
            self.inner.changed.notify_all();
        }
        removed
    }

    /// Force-release every manual lock overlapping `dir`, regardless of owner.
    ///
    /// This is used by the user-facing slash action for recovery from stale or
    /// mistaken manual locks. Automatic locks held by running tools are not
    /// touched.
    pub(crate) fn force_unlock_overlapping(&self, dir: &Path) -> Vec<ForceUnlockedLock> {
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        let mut removed = Vec::new();
        state.manual.retain(|lock| {
            let should_remove = paths_overlap(&lock.dir, dir);
            if should_remove {
                removed.push(ForceUnlockedLock {
                    owner: lock.owner.clone(),
                    dir: lock.dir.clone(),
                });
            }
            !should_remove
        });
        if !removed.is_empty() {
            self.inner.changed.notify_all();
        }
        removed
    }

    /// Release all manual locks owned by an unloaded agent.
    pub(crate) fn release_agent(&self, owner: &AgentId) -> usize {
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        let before_manual = state.manual.len();
        let before_waiters = state.waiters.len();
        state.manual.retain(|lock| &lock.owner != owner);
        state.waiters.retain(|waiter| &waiter.owner != owner);
        let removed = before_manual - state.manual.len();
        let cancelled = before_waiters - state.waiters.len();
        if 0 < removed + cancelled {
            self.inner.changed.notify_all();
        }
        removed
    }

    /// Release all manual locks and cancel queued lock waiters.
    ///
    /// This is the directory-lock shutdown cleanup used before the worker
    /// scheduler is dropped. Running scheduler jobs can be blocked inside
    /// `acquire_auto`/`acquire_manual`; clearing waiters wakes those jobs so
    /// scheduler shutdown can join worker threads deterministically.
    pub(crate) fn shutdown(&self) -> (usize, usize) {
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        let removed = state.manual.len();
        let cancelled = state.waiters.len();
        state.manual.clear();
        state.waiters.clear();
        if 0 < removed + cancelled {
            self.inner.changed.notify_all();
        }
        (removed, cancelled)
    }

    /// Disable directory locking by releasing manual locks and cancelling
    /// queued waiters.
    pub(crate) fn disable(&self) -> (usize, usize) {
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        let removed_manual = state.manual.len();
        let cancelled_waiters = state.waiters.len();
        state.manual.clear();
        state.waiters.clear();
        if 0 < removed_manual || 0 < cancelled_waiters {
            self.inner.changed.notify_all();
        }
        (removed_manual, cancelled_waiters)
    }

    fn release_auto(&self, id: u64) {
        let mut state = self.inner.state.lock().expect("dir lock state poisoned");
        let before = state.automatic.len();
        state.automatic.retain(|lock| lock.id != id);
        if state.automatic.len() != before {
            state.mark_auto_released(id, Instant::now());
            self.inner.changed.notify_all();
        }
    }
}

impl LockState {
    fn push_waiter(
        &mut self,
        call_id: ToolCallId,
        owner: AgentId,
        dirs: Vec<PathBuf>,
        kind: WaitKind,
    ) -> u64 {
        let id = self.next_waiter_id;
        self.next_waiter_id += 1;
        self.waiters.push_back(Waiter {
            id,
            call_id,
            owner,
            dirs,
            kind,
        });
        id
    }

    fn manual_lock_owned_overlapping(&self, owner: &AgentId, dirs: &[PathBuf]) -> Option<PathBuf> {
        self.manual.iter().find_map(|lock| {
            (&lock.owner == owner && dirs.iter().any(|dir| paths_overlap(&lock.dir, dir)))
                .then(|| lock.dir.clone())
        })
    }

    fn can_grant_now(&self, owner: &AgentId, dirs: &[PathBuf], kind: WaitKind) -> bool {
        let bypass_queue = self.can_bypass_queue(owner, dirs, kind);
        (bypass_queue || self.waiters.is_empty()) && !self.has_conflict(owner, dirs, kind)
    }

    fn can_bypass_queue(&self, owner: &AgentId, dirs: &[PathBuf], kind: WaitKind) -> bool {
        match kind {
            WaitKind::Manual => false,
            WaitKind::Automatic => self.manual_covers(owner, dirs),
        }
    }

    fn manual_covers(&self, owner: &AgentId, dirs: &[PathBuf]) -> bool {
        dirs.iter().all(|dir| {
            self.manual
                .iter()
                .any(|lock| &lock.owner == owner && dir.starts_with(&lock.dir))
        })
    }

    fn has_conflict(&self, owner: &AgentId, dirs: &[PathBuf], kind: WaitKind) -> bool {
        let manual_reentry = kind == WaitKind::Automatic && self.manual_covers(owner, dirs);
        if self.automatic.iter().any(|lock| {
            let same_owner_reentry =
                manual_reentry && &lock.owner == owner && self.manual_covers(owner, &lock.dirs);
            !same_owner_reentry
                && lock
                    .dirs
                    .iter()
                    .any(|active| dirs.iter().any(|dir| paths_overlap(active, dir)))
        }) {
            return true;
        }

        self.manual.iter().any(|lock| {
            let same_owner_reentry =
                &lock.owner == owner && kind == WaitKind::Automatic && manual_reentry;
            if &lock.owner == owner && (kind == WaitKind::Manual || same_owner_reentry) {
                return false;
            }
            match kind {
                WaitKind::Manual | WaitKind::Automatic => {
                    dirs.iter().any(|dir| paths_overlap(&lock.dir, dir))
                }
            }
        })
    }

    fn abandoned_blocker(
        &self,
        owner: &AgentId,
        dirs: &[PathBuf],
        kind: WaitKind,
        now: Instant,
        abandoned_after: Duration,
    ) -> Option<AbandonedLock> {
        match kind {
            WaitKind::Manual | WaitKind::Automatic => self.manual.iter().find_map(|lock| {
                if &lock.owner == owner || !dirs.iter().any(|dir| paths_overlap(&lock.dir, dir)) {
                    return None;
                }
                if !lock.active_auto_ids.is_empty() {
                    return None;
                }
                let idle_for = now.saturating_duration_since(lock.last_used_at);
                if idle_for < abandoned_after {
                    return None;
                }
                Some(AbandonedLock {
                    owner: lock.owner.clone(),
                    dir: lock.dir.clone(),
                    held_for: now.saturating_duration_since(lock.acquired_at),
                    idle_for,
                })
            }),
        }
    }

    fn add_manual(&mut self, owner: AgentId, dirs: Vec<PathBuf>, now: Instant) {
        for dir in dirs {
            debug_assert!(
                self.manual
                    .iter()
                    .all(|lock| lock.owner != owner || !paths_overlap(&lock.dir, &dir))
            );
            self.manual.push(ManualLock {
                owner: owner.clone(),
                dir,
                acquired_at: now,
                last_used_at: now,
                active_auto_ids: Vec::new(),
            });
        }
    }

    fn add_auto(&mut self, owner: AgentId, dirs: Vec<PathBuf>) -> u64 {
        let id = self.next_auto_id;
        self.next_auto_id += 1;
        self.automatic.push(AutomaticLock { id, owner, dirs });
        self.mark_auto_acquired(id, Instant::now());
        id
    }

    fn mark_auto_acquired(&mut self, id: u64, now: Instant) {
        let Some(lock) = self.automatic.iter().find(|lock| lock.id == id) else {
            return;
        };
        for manual in &mut self.manual {
            if manual.owner == lock.owner
                && lock.dirs.iter().any(|dir| dir.starts_with(&manual.dir))
                && !manual.active_auto_ids.contains(&id)
            {
                manual.last_used_at = now;
                manual.active_auto_ids.push(id);
            }
        }
    }

    fn mark_auto_released(&mut self, id: u64, now: Instant) {
        for manual in &mut self.manual {
            let before = manual.active_auto_ids.len();
            manual.active_auto_ids.retain(|active_id| *active_id != id);
            if manual.active_auto_ids.len() != before {
                manual.last_used_at = now;
            }
        }
    }
}

/// Handle the agent-visible `dir_lock` tool and stream any waiting progress
/// before the lock is granted.
pub(crate) fn dispatch_dir_lock_tool(
    invoke: ToolStarted,
    manager: &DirLockManager,
    enabled: bool,
    tx: &mpsc::Sender<HarnessInputMessage>,
) {
    if !enabled {
        send_event(
            tx,
            tool_error(
                &invoke,
                "dir_lock is disabled; set ext-shell config `dir_lock.enable` to true to use it"
                    .to_owned(),
                None,
            ),
        );
        return;
    }
    if invoke.agent_id.is_empty() {
        send_event(
            tx,
            tool_error(
                &invoke,
                "dir_lock requires a non-empty tool owner agent_id".to_owned(),
                None,
            ),
        );
        return;
    }

    let command = match argument_text(&invoke.arguments, "command") {
        Ok(command) => command,
        Err(message) => {
            send_event(
                tx,
                tool_error(&invoke, message, Some(invoke.arguments.clone())),
            );
            return;
        }
    };
    let dir_arg = match argument_text(&invoke.arguments, "directory") {
        Ok(directory) => directory,
        Err(message) => {
            send_event(
                tx,
                tool_error(&invoke, message, Some(invoke.arguments.clone())),
            );
            return;
        }
    };
    let dir = match canonical_existing_dir(Path::new(&dir_arg)) {
        Ok(dir) => dir,
        Err(message) => {
            send_event(
                tx,
                tool_error_with_args(
                    &invoke,
                    message,
                    Some(invoke.arguments.clone()),
                    Some(dir_arg.clone()),
                ),
            );
            return;
        }
    };

    match command.as_str() {
        "update" => {
            let wait_invoke = invoke.clone();
            let wait_dir = dir.clone();
            let wait_tx = tx.clone();
            match manager.acquire_manual(
                invoke.call_id.clone(),
                invoke.agent_id.clone(),
                dir.clone(),
                move || {
                    send_event(
                        &wait_tx,
                        waiting_progress_event(&wait_invoke, &[wait_dir], None),
                    )
                },
            ) {
                Ok(()) => send_event(
                    tx,
                    tool_result(
                        &invoke,
                        dir_lock_result_value(&dir_arg, &dir, Some(true)),
                        dir_lock_display("update", &dir),
                    ),
                ),
                Err(ManualLockAcquireError::Cancelled) => {
                    send_event(tx, cancelled_event(invoke));
                }
                Err(ManualLockAcquireError::AlreadyHeld { dir: held_dir }) => send_event(
                    tx,
                    tool_error_with_args(
                        &invoke,
                        DUPLICATE_LOCK_ERROR.to_owned(),
                        Some(duplicate_manual_lock_details(
                            &invoke.agent_id,
                            &held_dir,
                            &dir,
                        )),
                        Some(dir_lock_display_args("update", &dir)),
                    ),
                ),
                Err(ManualLockAcquireError::Abandoned(lock)) => send_event(
                    tx,
                    tool_error_with_args(
                        &invoke,
                        lock.message(),
                        Some(lock.details()),
                        Some(dir_lock_display_args("update", &lock.dir)),
                    ),
                ),
            }
        }
        "unlock" => {
            let owner_arg = match optional_argument_text(&invoke.arguments, "owner_agent_id") {
                Ok(owner_arg) => owner_arg,
                Err(message) => {
                    send_event(
                        tx,
                        tool_error_with_args(
                            &invoke,
                            message,
                            Some(invoke.arguments.clone()),
                            Some(dir_lock_display_args("unlock", &dir)),
                        ),
                    );
                    return;
                }
            };
            let owner = match owner_arg.as_deref() {
                Some(owner) => match owner.parse::<AgentId>() {
                    Ok(owner) => owner,
                    Err(error) => {
                        send_event(
                            tx,
                            tool_error_with_args(
                                &invoke,
                                format!("invalid owner_agent_id `{owner}`: {error}"),
                                Some(invoke.arguments.clone()),
                                Some(dir_lock_display_args("unlock", &dir)),
                            ),
                        );
                        return;
                    }
                },
                None => invoke.agent_id.clone(),
            };
            match manager.unlock_manual(&owner, &dir) {
                Ok(()) => send_event(
                    tx,
                    tool_result(
                        &invoke,
                        dir_lock_result_value(&dir_arg, &dir, Some(false)),
                        dir_lock_display("unlock", &dir),
                    ),
                ),
                Err(message) => send_event(
                    tx,
                    tool_error_with_args(
                        &invoke,
                        message,
                        Some(invoke.arguments.clone()),
                        Some(dir_lock_display_args("unlock", &dir)),
                    ),
                ),
            }
        }
        _ => send_event(
            tx,
            tool_error_with_args(
                &invoke,
                "argument `command` must be `update` or `unlock`".to_owned(),
                Some(invoke.arguments.clone()),
                Some(dir_lock_display_args(command.as_str(), &dir)),
            ),
        ),
    }
}

/// Return the canonical update-lock directories for a mutating ext-shell tool.
pub(crate) fn automatic_lock_dirs_for_tool_in_dir(
    tool_name: &str,
    arguments: &CborValue,
    cwd: &Path,
) -> Result<Vec<PathBuf>, ToolFailure> {
    match tool_name {
        EDIT_TOOL_NAME => {
            let path = argument_text(arguments, "path").map_err(ToolFailure::from)?;
            Ok(vec![canonical_write_lock_dir(Path::new(&path))?])
        }
        SHELL_TOOL_NAME | GPT_SHELL_TOOL_NAME => Ok(vec![canonical_shell_cwd(arguments)?]),
        APPLY_PATCH_TOOL_NAME => Ok(crate::tools::apply_patch::lock_directories_in_dir(
            arguments, cwd,
        )?),
        _ => Err(ToolFailure::new(format!(
            "tool `{tool_name}` does not use automatic directory locks"
        ))),
    }
}

/// Build a progress event that replaces the live tool block while waiting for
/// a directory update lock.
pub(crate) fn waiting_progress_event(
    invoke: &ToolStarted,
    dirs: &[PathBuf],
    shell_command_mode: Option<crate::tools::shell::ShellCommandMode>,
) -> Event {
    let dirs_display = display_dirs(dirs);
    let mut display = match shell_command_mode {
        Some(mode) => crate::tools::shell::initial_display(&invoke.arguments, mode),
        None => crate::tools::initial_display(invoke).unwrap_or_else(|| ToolUseState {
            args: dirs_display.clone(),
            ..Default::default()
        }),
    };
    display.args = dirs_display.clone();
    display.info_chips.push("dir lock".to_owned());
    display.status = ToolUseStatus::InProgress;
    display.status_text = "waiting".to_owned();

    Event::ToolProgress(ToolProgress {
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        message: Some(format!("waiting for directory lock: {dirs_display}")),
        progress: None,
        display: Some(display),
    })
}
/// Canonicalize `path` as an existing directory.
pub(crate) fn canonical_existing_dir(path: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("directory {} does not exist: {error}", path.display()))?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| format!("failed to stat directory {}: {error}", canonical.display()))?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    Ok(canonical)
}

/// Return a stable human-readable lock directory list.
pub(crate) fn display_dirs(dirs: &[PathBuf]) -> String {
    dirs.iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Canonical write-target lock directory, following the final symlink chain
/// when the destination path is already a symlink. Missing parents lock the
/// deepest existing ancestor so `edit` can keep creating parent directories
/// safely.
pub(crate) fn canonical_write_lock_dir(path: &Path) -> Result<PathBuf, ToolFailure> {
    let lock_path = crate::tools::world::final_write_path(path).map_err(|error| {
        ToolFailure::from(format!("failed to resolve {}: {error}", path.display()))
            .with_args(path.display().to_string())
    })?;
    let parent = lock_path.parent().ok_or_else(|| {
        ToolFailure::from(format!(
            "path {} has no parent directory",
            lock_path.display()
        ))
        .with_args(path.display().to_string())
    })?;
    canonical_deepest_existing_ancestor(parent)
        .map_err(|message| ToolFailure::from(message).with_args(path.display().to_string()))
}

/// Canonical parent directory for an existing file, following symlinks to the
/// actual file that will be modified by `edit`.
pub(crate) fn canonical_existing_file_parent(path: &Path) -> Result<PathBuf, ToolFailure> {
    let canonical = path.canonicalize().map_err(|error| {
        ToolFailure::from(format!("file {} does not exist: {error}", path.display()))
            .with_args(path.display().to_string())
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        ToolFailure::from(format!(
            "failed to stat file {}: {error}",
            canonical.display()
        ))
        .with_args(path.display().to_string())
    })?;
    if metadata.is_dir() {
        return Err(ToolFailure::from(format!(
            "{} is a directory, not a file",
            canonical.display()
        ))
        .with_args(path.display().to_string()));
    }
    canonical.parent().map(Path::to_path_buf).ok_or_else(|| {
        ToolFailure::from(format!(
            "file {} has no parent directory",
            canonical.display()
        ))
        .with_args(path.display().to_string())
    })
}

/// Canonical lock directory for an apply_patch in-place update.
///
/// Existing final symlinks are followed because `fs::write` updates their
/// target. Missing files and directories lock the canonical requested parent so
/// apply_patch can preserve its normal partial-failure behavior.
pub(crate) fn canonical_update_lock_dir(path: &Path) -> Result<PathBuf, ToolFailure> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => canonical_existing_file_parent(path),
        Ok(_) => canonical_path_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => canonical_path_parent(path),
        Err(error) => Err(ToolFailure::from(format!(
            "failed to stat file {}: {error}",
            path.display()
        ))
        .with_args(path.display().to_string())),
    }
}

/// Canonical parent for a path whose final component may be removed or
/// replaced without following a final symlink.
pub(crate) fn canonical_path_parent(path: &Path) -> Result<PathBuf, ToolFailure> {
    let abs = absolute_path(path).map_err(|error| {
        ToolFailure::from(format!("failed to resolve {}: {error}", path.display()))
            .with_args(path.display().to_string())
    })?;
    let parent = abs.parent().ok_or_else(|| {
        ToolFailure::from(format!("path {} has no parent directory", abs.display()))
            .with_args(path.display().to_string())
    })?;
    canonical_existing_dir(parent)
        .map_err(|message| ToolFailure::from(message).with_args(path.display().to_string()))
}

/// Canonical lock directory for a shell command's cwd argument.
pub(crate) fn canonical_shell_cwd(arguments: &CborValue) -> Result<PathBuf, ToolFailure> {
    let cwd =
        crate::argument::optional_argument_text(arguments, "cwd").map_err(ToolFailure::from)?;
    let display_arg = cwd.clone().unwrap_or_else(|| ".".to_owned());
    let path = cwd
        .as_deref()
        .map(Path::new)
        .unwrap_or_else(|| Path::new("."));
    canonical_existing_dir(path)
        .map_err(|message| ToolFailure::from(message).with_args(display_arg))
}

/// Convert a possibly relative path to an absolute path without requiring the
/// final component to exist.
fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir().map(|cwd| cwd.join(path))
    }
}

fn canonical_deepest_existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    let mut candidate = path.to_path_buf();
    loop {
        match canonical_existing_dir(&candidate) {
            Ok(dir) => return Ok(dir),
            Err(_) => {
                if !candidate.pop() {
                    return Err(format!(
                        "no existing ancestor directory for {}",
                        path.display()
                    ));
                }
            }
        }
    }
}

pub(crate) fn normalize_lock_dirs(mut dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    dirs.sort_by(|a, b| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });
    dirs.dedup();
    let mut normalized: Vec<PathBuf> = Vec::new();
    'next: for dir in dirs {
        for existing in &normalized {
            if dir.starts_with(existing) {
                continue 'next;
            }
        }
        normalized.push(dir);
    }
    normalized
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn dir_lock_result_value(
    input_directory: &str,
    canonical_dir: &Path,
    locked: Option<bool>,
) -> CborValue {
    let canonical_directory = canonical_dir.display().to_string();
    let mut entries = Vec::new();
    if canonical_directory != input_directory {
        entries.push((
            CborValue::Text("canonical_directory".to_owned()),
            CborValue::Text(canonical_directory),
        ));
    }
    if let Some(locked) = locked {
        entries.push((
            CborValue::Text("locked".to_owned()),
            CborValue::Bool(locked),
        ));
    }
    CborValue::Map(entries)
}

fn dir_lock_display_args(command: &str, dir: &Path) -> String {
    format!("{command} {}", dir.display())
}

fn dir_lock_display(command: &str, dir: &Path) -> ToolUseState {
    ok_display(dir_lock_display_args(command, dir))
}

fn tool_result(invoke: &ToolStarted, result: CborValue, display: ToolUseState) -> Event {
    Event::ToolResult(ToolResult {
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: ToolType::Function,
        result,
        kind: ToolResultKind::Final,
        display: Some(display),
        originator: invoke.originator.clone(),
    })
}

fn tool_error(invoke: &ToolStarted, message: String, details: Option<CborValue>) -> Event {
    tool_error_with_args(invoke, message, details, None)
}

fn tool_error_with_args(
    invoke: &ToolStarted,
    message: String,
    details: Option<CborValue>,
    args: Option<String>,
) -> Event {
    Event::ToolError(ToolError {
        call_id: invoke.call_id.clone(),
        tool_name: invoke.tool_name.clone(),
        tool_type: ToolType::Function,
        message,
        details,
        display: Some(ToolUseState {
            args: args.unwrap_or_default(),
            status: ToolUseStatus::Error,
            status_text: "dir_lock failed".to_owned(),
            ..Default::default()
        }),
        originator: invoke.originator.clone(),
    })
}

fn cancelled_event(invoke: ToolStarted) -> Event {
    Event::ToolCancelled(ToolCancelled {
        call_id: invoke.call_id,
        tool_name: invoke.tool_name,
        tool_type: ToolType::Function,
    })
}

fn send_event(tx: &mpsc::Sender<HarnessInputMessage>, event: Event) {
    let _ = tx.send(HarnessInputMessage::emit(event));
}

#[cfg(test)]
mod tests;

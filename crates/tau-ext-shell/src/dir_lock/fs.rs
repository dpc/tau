//! Filesystem-backed directory lock registry for `tau-ext-shell`.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tau_proto::{AgentId, ToolCallId};

use super::{
    AbandonedLock, AutoDirLockGuard, AutoLockToken, DirLockManager, ForceUnlockedLock,
    LockAcquireError, LockWaitPolicy, ManualLockAcquireError, UnlockOwnerScope, WaitKind,
    normalize_lock_dirs, paths_overlap,
};

const FS_REGISTRY_VERSION: u32 = 1;
const FS_WAIT_POLL_INITIAL_INTERVAL: Duration = Duration::from_millis(50);
const FS_WAIT_POLL_MAX_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(test)]
static FAIL_REAP_FOR_TEST: std::sync::LazyLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// Filesystem-backed lock registry plus this instance's lease handle.
#[derive(Clone, Debug)]
pub(super) struct FsLockBackend {
    /// Directory containing `registry.lock`, `registry.json`, and leases.
    pub(super) state_dir: PathBuf,
    /// Internal process-instance lease identifier, never shown to users.
    instance_id: FsInstanceId,
    /// RAII lease file: held exclusively while this backend or any guard clone
    /// lives.
    _lease: Arc<File>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FsInstanceId(String);

impl FsInstanceId {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StateDirSource {
    Default,
    Configured,
}

impl StateDirSource {
    fn reject_existing_insecure(self) -> bool {
        self == Self::Configured
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FsRegistry {
    /// Persistent registry format version used to reject incompatible state.
    version: u32,
    /// Monotonic change counter bumped whenever persisted lock state changes.
    generation: u64,
    /// Next FIFO waiter id, unique within this registry file.
    next_waiter_id: u64,
    /// Next automatic lock id, unique within this registry file.
    next_auto_id: u64,
    /// Manual locks retained until explicit unlock, owner cleanup, or lease
    /// reap.
    manual: Vec<FsManualLock>,
    /// Automatic locks held only for the lifetime of active mutating tool
    /// calls.
    automatic: Vec<FsAutomaticLock>,
    /// FIFO queue of blocked manual and automatic acquisitions.
    waiters: VecDeque<FsWaiter>,
}

impl Default for FsRegistry {
    fn default() -> Self {
        Self {
            version: FS_REGISTRY_VERSION,
            generation: 0,
            next_waiter_id: 0,
            next_auto_id: 0,
            manual: Vec::new(),
            automatic: Vec::new(),
            waiters: VecDeque::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FsOwner {
    /// Internal ext-shell process lease id that disambiguates equal agent ids.
    instance_id: FsInstanceId,
    /// User-visible agent id used in diagnostics and explicit owner unlocks.
    agent_id: AgentId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FsManualLock {
    /// Instance/agent pair that owns this manual lock.
    owner: FsOwner,
    /// Canonical directory reserved by this manual lock.
    dir: PathBuf,
    /// Registry-wall-clock acquisition timestamp for stale-lock diagnostics.
    acquired_at_ms: u64,
    /// Last acquisition or same-owner automatic use for abandonment detection.
    last_used_at_ms: u64,
    /// Same-owner automatic lock ids currently running under this manual lock.
    active_auto_ids: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FsAutomaticLock {
    /// Automatic lock id referenced by manual `active_auto_ids`.
    id: u64,
    /// Instance/agent pair that owns the active mutating tool.
    owner: FsOwner,
    /// Canonical directories covered by this automatic lock.
    dirs: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct FsWaiter {
    /// FIFO waiter id persisted so pollers can find their queue entry.
    id: u64,
    /// Tool call id used by cancellation to remove this queued waiter.
    call_id: ToolCallId,
    /// Instance/agent pair waiting for the lock.
    owner: FsOwner,
    /// Canonical directories requested by this waiter.
    dirs: Vec<PathBuf>,
    /// Whether this queued request is a manual lock or automatic tool lock.
    kind: WaitKind,
}

pub(super) struct FsAutoAcquireRequest<'a> {
    pub(super) manager: &'a DirLockManager,
    pub(super) call_id: ToolCallId,
    pub(super) agent_id: AgentId,
    pub(super) dirs: Vec<PathBuf>,
    pub(super) require_manual_cover: bool,
}

pub(super) struct FsManualAcquireRequest<'a> {
    pub(super) manager: &'a DirLockManager,
    pub(super) call_id: ToolCallId,
    pub(super) agent_id: AgentId,
    pub(super) dir: PathBuf,
}

impl FsLockBackend {
    pub(super) fn initialize(state_dir: &Path, source: StateDirSource) -> Result<Self, String> {
        let state_dir = state_dir.to_path_buf();
        ensure_private_state_dir(&state_dir, source)?;
        fs::create_dir_all(state_dir.join("instances")).map_err(|error| {
            format!(
                "failed to create dir_lock instances directory under {}: {error}",
                state_dir.display()
            )
        })?;
        let instance_id = claim_instance_id(&state_dir)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(instance_lock_path(&state_dir, &instance_id))
            .map_err(|error| format!("failed to open dir_lock instance lease: {error}"))?;
        lease
            .lock_exclusive()
            .map_err(|error| format!("failed to lock dir_lock instance lease: {error}"))?;
        with_registry_lock(&state_dir, |registry| {
            registry.generation = registry.generation.saturating_add(1);
            Ok(())
        })?;
        Ok(Self {
            state_dir,
            instance_id,
            _lease: Arc::new(lease),
        })
    }

    fn owner(&self, agent_id: AgentId) -> FsOwner {
        FsOwner {
            instance_id: self.instance_id.clone(),
            agent_id,
        }
    }

    pub(super) fn same_instance_as(&self, other: &Self) -> bool {
        self.state_dir == other.state_dir && self.instance_id == other.instance_id
    }

    pub(super) fn acquire_auto<F>(
        &self,
        request: FsAutoAcquireRequest<'_>,
        on_wait: F,
        policy: LockWaitPolicy,
        admission_guard: MutexGuard<'_, ()>,
    ) -> Result<AutoDirLockGuard, LockAcquireError>
    where
        F: FnOnce(),
    {
        let owner = self.owner(request.agent_id);
        let dirs = normalize_lock_dirs(request.dirs);
        let mut waiter_id = None;
        let mut on_wait = Some(on_wait);
        let mut next_liveness_check = Instant::now() + policy.liveness_interval;
        let mut wait_backoff = FsWaitBackoff::default();
        let mut admission_guard = Some(admission_guard);
        loop {
            // Snapshot before registry observation so same-process wakes that
            // land between the registry check and timed sleep are not lost.
            let observed_wake_generation = request.manager.wake_generation();
            let mut wake_waiters = false;
            let outcome = with_registry_lock(&self.state_dir, |registry| {
                self.reap_dead_instances(registry)?;
                if waiter_id.is_some()
                    && admission_guard.is_some()
                    && !request.manager.is_current_fs_backend(self)
                {
                    registry.remove_waiter(waiter_id);
                    wake_waiters = true;
                    return Ok(FsAcquireOutcome::Cancelled);
                }
                if request.require_manual_cover && !registry.manual_covers(&owner, &dirs) {
                    registry.remove_waiter(waiter_id);
                    wake_waiters = waiter_id.is_some();
                    return Ok(FsAcquireOutcome::NotCovered);
                }
                if !registry.manual_covers(&owner, &dirs)
                    && let Some(dir) = registry.manual_lock_owned_overlapping(&owner, &dirs)
                {
                    return Ok(FsAcquireOutcome::SelfConflict(dir));
                }
                if waiter_id.is_none() && registry.can_grant_now(&owner, &dirs, WaitKind::Automatic)
                {
                    let id = registry.add_auto(owner.clone(), dirs.clone(), now_ms());
                    return Ok(FsAcquireOutcome::Granted(id));
                }
                let id = *waiter_id.get_or_insert_with(|| {
                    registry.push_waiter(
                        request.call_id.clone(),
                        owner.clone(),
                        dirs.clone(),
                        WaitKind::Automatic,
                    )
                });
                let Some(pos) = registry.waiters.iter().position(|queued| queued.id == id) else {
                    return Ok(FsAcquireOutcome::Cancelled);
                };
                if pos == 0 {
                    let queued = registry.waiters.front().expect("front waiter");
                    if request.require_manual_cover
                        && !registry.manual_covers(&queued.owner, &queued.dirs)
                    {
                        registry.waiters.pop_front();
                        registry.bump();
                        wake_waiters = true;
                        return Ok(FsAcquireOutcome::NotCovered);
                    }
                    if !registry.has_conflict(&queued.owner, &queued.dirs, queued.kind) {
                        if admission_guard.is_none() {
                            return Ok(FsAcquireOutcome::NeedsAdmission);
                        }
                        let queued = registry.waiters.pop_front().expect("front waiter");
                        let id = registry.add_auto(queued.owner, queued.dirs, now_ms());
                        wake_waiters = true;
                        return Ok(FsAcquireOutcome::Granted(id));
                    }
                    if next_liveness_check <= Instant::now()
                        && let Some(blocker) = registry.abandoned_blocker(
                            &queued.owner,
                            &queued.dirs,
                            queued.kind,
                            now_ms(),
                            policy.abandoned_after,
                        )
                    {
                        registry.waiters.pop_front();
                        registry.bump();
                        wake_waiters = true;
                        return Ok(FsAcquireOutcome::Abandoned(blocker));
                    }
                }
                Ok(FsAcquireOutcome::Waiting)
            })
            .map_err(|error| {
                cleanup_waiter_after_backend_error(self, waiter_id);
                LockAcquireError::Backend(error)
            })?;
            if wake_waiters {
                request.manager.notify_lock_waiters();
            }
            drop(admission_guard.take());
            match outcome {
                FsAcquireOutcome::Granted(id) => {
                    return Ok(AutoDirLockGuard {
                        manager: request.manager.clone(),
                        token: AutoLockToken::Filesystem {
                            backend: self.clone(),
                            id,
                        },
                    });
                }
                FsAcquireOutcome::Cancelled => return Err(LockAcquireError::Cancelled),
                FsAcquireOutcome::Abandoned(lock) => return Err(LockAcquireError::Abandoned(lock)),
                FsAcquireOutcome::SelfConflict(dir) => {
                    return Err(LockAcquireError::SelfConflict { dir });
                }
                FsAcquireOutcome::NotCovered => return Err(LockAcquireError::NotCovered),
                FsAcquireOutcome::NeedsAdmission => {
                    admission_guard = Some(request.manager.backend_admission_gate());
                    continue;
                }
                FsAcquireOutcome::Waiting => {}
            }
            if let Some(on_wait) = on_wait.take() {
                on_wait();
            }
            if next_liveness_check <= Instant::now() {
                next_liveness_check = Instant::now() + policy.liveness_interval;
            }
            wait_for_lock_change(
                request.manager,
                next_liveness_check,
                &mut wait_backoff,
                observed_wake_generation,
            );
        }
    }

    pub(super) fn acquire_manual<F>(
        &self,
        request: FsManualAcquireRequest<'_>,
        on_wait: F,
        policy: LockWaitPolicy,
        admission_guard: MutexGuard<'_, ()>,
    ) -> Result<(), ManualLockAcquireError>
    where
        F: FnOnce(),
    {
        let owner = self.owner(request.agent_id);
        let dirs = vec![request.dir];
        let mut waiter_id = None;
        let mut on_wait = Some(on_wait);
        let mut next_liveness_check = Instant::now() + policy.liveness_interval;
        let mut wait_backoff = FsWaitBackoff::default();
        let mut admission_guard = Some(admission_guard);
        loop {
            // Snapshot before registry observation so same-process wakes that
            // land between the registry check and timed sleep are not lost.
            let observed_wake_generation = request.manager.wake_generation();
            let mut wake_waiters = false;
            let outcome = with_registry_lock(&self.state_dir, |registry| {
                self.reap_dead_instances(registry)?;
                if waiter_id.is_some()
                    && admission_guard.is_some()
                    && !request.manager.is_current_fs_backend(self)
                {
                    registry.remove_waiter(waiter_id);
                    wake_waiters = true;
                    return Ok(FsManualOutcome::Cancelled);
                }
                if let Some(held_dir) = registry.manual_lock_owned_overlapping(&owner, &dirs) {
                    registry.remove_waiter(waiter_id);
                    wake_waiters = waiter_id.is_some();
                    return Ok(FsManualOutcome::AlreadyHeld(held_dir));
                }
                if waiter_id.is_none() && registry.can_grant_now(&owner, &dirs, WaitKind::Manual) {
                    registry.add_manual(owner.clone(), dirs.clone(), now_ms());
                    return Ok(FsManualOutcome::Granted);
                }
                let id = *waiter_id.get_or_insert_with(|| {
                    registry.push_waiter(
                        request.call_id.clone(),
                        owner.clone(),
                        dirs.clone(),
                        WaitKind::Manual,
                    )
                });
                let Some(pos) = registry.waiters.iter().position(|queued| queued.id == id) else {
                    return Ok(FsManualOutcome::Cancelled);
                };
                if pos == 0 {
                    let queued = registry.waiters.front().expect("front waiter");
                    if let Some(held_dir) =
                        registry.manual_lock_owned_overlapping(&queued.owner, &queued.dirs)
                    {
                        registry.waiters.pop_front();
                        registry.bump();
                        wake_waiters = true;
                        return Ok(FsManualOutcome::AlreadyHeld(held_dir));
                    }
                    if !registry.has_conflict(&queued.owner, &queued.dirs, queued.kind) {
                        if admission_guard.is_none() {
                            return Ok(FsManualOutcome::NeedsAdmission);
                        }
                        let queued = registry.waiters.pop_front().expect("front waiter");
                        registry.add_manual(queued.owner, queued.dirs, now_ms());
                        wake_waiters = true;
                        return Ok(FsManualOutcome::Granted);
                    }
                    if next_liveness_check <= Instant::now()
                        && let Some(blocker) = registry.abandoned_blocker(
                            &queued.owner,
                            &queued.dirs,
                            queued.kind,
                            now_ms(),
                            policy.abandoned_after,
                        )
                    {
                        registry.waiters.pop_front();
                        registry.bump();
                        wake_waiters = true;
                        return Ok(FsManualOutcome::Abandoned(blocker));
                    }
                }
                Ok(FsManualOutcome::Waiting)
            })
            .map_err(|error| {
                cleanup_waiter_after_backend_error(self, waiter_id);
                ManualLockAcquireError::Backend(error)
            })?;
            if wake_waiters {
                request.manager.notify_lock_waiters();
            }
            match outcome {
                FsManualOutcome::Granted => return Ok(()),
                FsManualOutcome::Cancelled => return Err(ManualLockAcquireError::Cancelled),
                FsManualOutcome::AlreadyHeld(dir) => {
                    return Err(ManualLockAcquireError::AlreadyHeld { dir });
                }
                FsManualOutcome::Abandoned(lock) => {
                    return Err(ManualLockAcquireError::Abandoned(lock));
                }
                FsManualOutcome::NeedsAdmission => {
                    admission_guard = Some(request.manager.backend_admission_gate());
                    continue;
                }
                FsManualOutcome::Waiting => {}
            }
            drop(admission_guard.take());
            if let Some(on_wait) = on_wait.take() {
                on_wait();
            }
            if next_liveness_check <= Instant::now() {
                next_liveness_check = Instant::now() + policy.liveness_interval;
            }
            wait_for_lock_change(
                request.manager,
                next_liveness_check,
                &mut wait_backoff,
                observed_wake_generation,
            );
        }
    }

    pub(super) fn unlock_manual(
        &self,
        agent_id: &AgentId,
        dir: &Path,
        scope: UnlockOwnerScope,
    ) -> Result<(), String> {
        let owner = FsOwner {
            instance_id: self.instance_id.clone(),
            agent_id: agent_id.clone(),
        };
        with_registry_lock(&self.state_dir, |registry| {
            self.reap_dead_instances(registry)?;
            let pos = registry.manual.iter().position(|lock| {
                lock.dir == dir
                    && match scope {
                        UnlockOwnerScope::AnyInstanceWithAgentId => {
                            &lock.owner.agent_id == agent_id
                        }
                        UnlockOwnerScope::CurrentInstance => lock.owner == owner,
                    }
            });
            let Some(pos) = pos else {
                return Err(format!(
                    "agent `{agent_id}` does not hold a directory lock for {}",
                    dir.display()
                ));
            };
            registry.manual.remove(pos);
            registry.bump();
            Ok(())
        })
    }

    pub(super) fn cancel_waiting_call(&self, call_id: &ToolCallId) -> bool {
        with_registry_lock(&self.state_dir, |registry| {
            let before = registry.waiters.len();
            registry.waiters.retain(|waiter| {
                !(waiter.owner.instance_id == self.instance_id && &waiter.call_id == call_id)
            });
            if registry.waiters.len() != before {
                registry.bump();
                return Ok(true);
            }
            Ok(false)
        })
        .unwrap_or(false)
    }

    pub(super) fn force_unlock_overlapping(
        &self,
        dir: &Path,
    ) -> Result<Vec<ForceUnlockedLock>, String> {
        with_registry_lock(&self.state_dir, |registry| {
            self.reap_dead_instances(registry)?;
            let mut removed = Vec::new();
            registry.manual.retain(|lock| {
                let should_remove = paths_overlap(&lock.dir, dir);
                if should_remove {
                    removed.push(ForceUnlockedLock {
                        owner: lock.owner.agent_id.clone(),
                        dir: lock.dir.clone(),
                    });
                }
                !should_remove
            });
            if !removed.is_empty() {
                registry.bump();
            }
            Ok(removed)
        })
    }

    pub(super) fn active_auto_count(&self) -> Result<usize, String> {
        with_registry_lock(&self.state_dir, |registry| {
            self.reap_dead_instances(registry)?;
            Ok(registry
                .automatic
                .iter()
                .filter(|lock| lock.owner.instance_id == self.instance_id)
                .count())
        })
    }

    pub(super) fn release_agent(&self, agent_id: &AgentId) -> (usize, usize) {
        with_registry_lock(&self.state_dir, |registry| {
            let before_manual = registry.manual.len();
            let before_waiters = registry.waiters.len();
            registry.manual.retain(|lock| {
                !(lock.owner.instance_id == self.instance_id && &lock.owner.agent_id == agent_id)
            });
            registry.waiters.retain(|waiter| {
                !(waiter.owner.instance_id == self.instance_id
                    && &waiter.owner.agent_id == agent_id)
            });
            let removed = before_manual - registry.manual.len();
            let cancelled = before_waiters - registry.waiters.len();
            if removed + cancelled > 0 {
                registry.bump();
            }
            Ok((removed, cancelled))
        })
        .unwrap_or((0, 0))
    }

    pub(super) fn shutdown(&self) -> (usize, usize) {
        with_registry_lock(&self.state_dir, |registry| {
            let before_manual = registry.manual.len();
            let before_waiters = registry.waiters.len();
            registry
                .manual
                .retain(|lock| lock.owner.instance_id != self.instance_id);
            registry
                .waiters
                .retain(|waiter| waiter.owner.instance_id != self.instance_id);
            let removed = before_manual - registry.manual.len();
            let cancelled = before_waiters - registry.waiters.len();
            if removed + cancelled > 0 {
                registry.bump();
            }
            Ok((removed, cancelled))
        })
        .unwrap_or((0, 0))
    }

    pub(super) fn release_auto(&self, id: u64) {
        let _ = with_registry_lock(&self.state_dir, |registry| {
            let before = registry.automatic.len();
            registry
                .automatic
                .retain(|lock| !(lock.owner.instance_id == self.instance_id && lock.id == id));
            if registry.automatic.len() != before {
                registry.mark_auto_released(id, now_ms());
                registry.bump();
            }
            Ok(())
        });
    }

    fn reap_dead_instances(&self, registry: &mut FsRegistry) -> Result<(), String> {
        #[cfg(test)]
        if FAIL_REAP_FOR_TEST
            .lock()
            .expect("test reap failure mutex")
            .as_ref()
            .is_some_and(|state_dir| state_dir == &self.state_dir)
        {
            return Err("injected dir_lock reap failure".to_owned());
        }

        let instances: Vec<FsInstanceId> = registry
            .manual
            .iter()
            .map(|lock| lock.owner.instance_id.clone())
            .chain(
                registry
                    .automatic
                    .iter()
                    .map(|lock| lock.owner.instance_id.clone()),
            )
            .chain(
                registry
                    .waiters
                    .iter()
                    .map(|waiter| waiter.owner.instance_id.clone()),
            )
            .filter(|instance| instance != &self.instance_id)
            .collect();
        let mut dead = Vec::new();
        for instance in instances {
            if dead.contains(&instance) {
                continue;
            }
            match instance_liveness(&self.state_dir, &instance)? {
                InstanceLiveness::Dead => dead.push(instance),
                InstanceLiveness::Alive => {}
            }
        }
        if dead.is_empty() {
            return Ok(());
        }
        registry
            .manual
            .retain(|lock| !dead.contains(&lock.owner.instance_id));
        registry
            .automatic
            .retain(|lock| !dead.contains(&lock.owner.instance_id));
        registry
            .waiters
            .retain(|waiter| !dead.contains(&waiter.owner.instance_id));
        registry.bump();
        Ok(())
    }
}

enum FsAcquireOutcome {
    Granted(u64),
    Waiting,
    NeedsAdmission,
    Cancelled,
    Abandoned(AbandonedLock),
    SelfConflict(PathBuf),
    NotCovered,
}

enum FsManualOutcome {
    Granted,
    Waiting,
    NeedsAdmission,
    Cancelled,
    AlreadyHeld(PathBuf),
    Abandoned(AbandonedLock),
}

impl FsRegistry {
    fn bump(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }

    fn push_waiter(
        &mut self,
        call_id: ToolCallId,
        owner: FsOwner,
        dirs: Vec<PathBuf>,
        kind: WaitKind,
    ) -> u64 {
        let id = self.next_waiter_id;
        self.next_waiter_id = self.next_waiter_id.saturating_add(1);
        self.waiters.push_back(FsWaiter {
            id,
            call_id,
            owner,
            dirs,
            kind,
        });
        self.bump();
        id
    }

    fn manual_lock_owned_overlapping(&self, owner: &FsOwner, dirs: &[PathBuf]) -> Option<PathBuf> {
        self.manual.iter().find_map(|lock| {
            (&lock.owner == owner && dirs.iter().any(|dir| paths_overlap(&lock.dir, dir)))
                .then(|| lock.dir.clone())
        })
    }

    fn can_grant_now(&self, owner: &FsOwner, dirs: &[PathBuf], kind: WaitKind) -> bool {
        let bypass_queue = kind == WaitKind::Automatic && self.manual_covers(owner, dirs);
        (bypass_queue || self.waiters.is_empty()) && !self.has_conflict(owner, dirs, kind)
    }

    fn manual_covers(&self, owner: &FsOwner, dirs: &[PathBuf]) -> bool {
        dirs.iter().all(|dir| {
            self.manual
                .iter()
                .any(|lock| &lock.owner == owner && dir.starts_with(&lock.dir))
        })
    }

    fn has_conflict(&self, owner: &FsOwner, dirs: &[PathBuf], kind: WaitKind) -> bool {
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
            dirs.iter().any(|dir| paths_overlap(&lock.dir, dir))
        })
    }

    fn abandoned_blocker(
        &self,
        owner: &FsOwner,
        dirs: &[PathBuf],
        _kind: WaitKind,
        now_ms: u64,
        abandoned_after: Duration,
    ) -> Option<AbandonedLock> {
        self.manual.iter().find_map(|lock| {
            if &lock.owner == owner || !dirs.iter().any(|dir| paths_overlap(&lock.dir, dir)) {
                return None;
            }
            if !lock.active_auto_ids.is_empty() {
                return None;
            }
            let idle_for = Duration::from_millis(now_ms.saturating_sub(lock.last_used_at_ms));
            if idle_for < abandoned_after {
                return None;
            }
            Some(AbandonedLock {
                owner: lock.owner.agent_id.clone(),
                dir: lock.dir.clone(),
                held_for: Duration::from_millis(now_ms.saturating_sub(lock.acquired_at_ms)),
                idle_for,
            })
        })
    }

    fn add_manual(&mut self, owner: FsOwner, dirs: Vec<PathBuf>, now_ms: u64) {
        for dir in dirs {
            self.manual.push(FsManualLock {
                owner: owner.clone(),
                dir,
                acquired_at_ms: now_ms,
                last_used_at_ms: now_ms,
                active_auto_ids: Vec::new(),
            });
        }
        self.bump();
    }

    fn add_auto(&mut self, owner: FsOwner, dirs: Vec<PathBuf>, now_ms: u64) -> u64 {
        let id = self.next_auto_id;
        self.next_auto_id = self.next_auto_id.saturating_add(1);
        self.automatic.push(FsAutomaticLock { id, owner, dirs });
        self.mark_auto_acquired(id, now_ms);
        self.bump();
        id
    }

    fn mark_auto_acquired(&mut self, id: u64, now_ms: u64) {
        let Some(lock) = self.automatic.iter().find(|lock| lock.id == id) else {
            return;
        };
        for manual in &mut self.manual {
            if manual.owner == lock.owner
                && lock.dirs.iter().any(|dir| dir.starts_with(&manual.dir))
                && !manual.active_auto_ids.contains(&id)
            {
                manual.last_used_at_ms = now_ms;
                manual.active_auto_ids.push(id);
            }
        }
    }

    fn mark_auto_released(&mut self, id: u64, now_ms: u64) {
        for manual in &mut self.manual {
            let before = manual.active_auto_ids.len();
            manual.active_auto_ids.retain(|active_id| *active_id != id);
            if manual.active_auto_ids.len() != before {
                manual.last_used_at_ms = now_ms;
            }
        }
    }

    fn remove_waiter(&mut self, waiter_id: Option<u64>) {
        let Some(waiter_id) = waiter_id else {
            return;
        };
        let before = self.waiters.len();
        self.waiters.retain(|waiter| waiter.id != waiter_id);
        if self.waiters.len() != before {
            self.bump();
        }
    }
}

/// Adaptive delay used when filesystem waiters must re-check cross-process
/// registry state without a peer notification.
#[derive(Debug)]
struct FsWaitBackoff {
    /// Duration for the next timed cross-process registry re-check.
    next_delay: Duration,
}

impl Default for FsWaitBackoff {
    fn default() -> Self {
        Self {
            next_delay: FS_WAIT_POLL_INITIAL_INTERVAL,
        }
    }
}

impl FsWaitBackoff {
    /// Return the current delay, then grow future cross-process waits up to the
    /// configured ceiling.
    fn advance(&mut self) -> Duration {
        let current = self.next_delay;
        self.next_delay = (self.next_delay * 2).min(FS_WAIT_POLL_MAX_INTERVAL);
        current
    }

    /// Reset timed polling after an in-process condition-variable wake.
    fn reset(&mut self) {
        self.next_delay = FS_WAIT_POLL_INITIAL_INTERVAL;
    }
}

fn wait_for_lock_change(
    manager: &DirLockManager,
    next_liveness_check: Instant,
    backoff: &mut FsWaitBackoff,
    observed_wake_generation: u64,
) {
    let guard = manager.inner.state.lock().expect("dir lock state poisoned");
    if guard.wake_generation != observed_wake_generation {
        backoff.reset();
        return;
    }
    let now = Instant::now();
    // Timed waits intentionally consume backoff even when the liveness deadline
    // caps the actual sleep. The liveness deadline remains the faster cadence in
    // that case, while normal cross-process availability checks back off.
    let wait_for =
        select_wait_duration(backoff, next_liveness_check.saturating_duration_since(now));
    let (guard, wait_timeout) = manager
        .inner
        .changed
        .wait_timeout_while(guard, wait_for, |state| {
            state.wake_generation == observed_wake_generation
        })
        .expect("dir lock state poisoned");
    if guard.wake_generation != observed_wake_generation || !wait_timeout.timed_out() {
        backoff.reset();
    }
}

fn select_wait_duration(backoff: &mut FsWaitBackoff, until_liveness_check: Duration) -> Duration {
    backoff.advance().min(until_liveness_check)
}

#[cfg(test)]
pub(super) fn wait_backoff_delays_for_test(count: usize) -> Vec<Duration> {
    let mut backoff = FsWaitBackoff::default();
    (0..count).map(|_| backoff.advance()).collect()
}

#[cfg(test)]
pub(super) fn wait_after_observed_wake_for_test(manager: &DirLockManager) -> (Duration, Duration) {
    let observed_wake_generation = manager.wake_generation();
    let mut backoff = FsWaitBackoff {
        next_delay: FS_WAIT_POLL_MAX_INTERVAL,
    };
    manager.notify_lock_waiters();
    let started = Instant::now();
    wait_for_lock_change(
        manager,
        started + Duration::from_secs(60),
        &mut backoff,
        observed_wake_generation,
    );
    (started.elapsed(), backoff.advance())
}

#[cfg(test)]
pub(super) fn liveness_cap_consumes_backoff_for_test() -> (Duration, Duration) {
    let mut backoff = FsWaitBackoff {
        next_delay: Duration::from_millis(500),
    };
    let selected = select_wait_duration(&mut backoff, Duration::from_millis(5));
    let next_delay = backoff.advance();
    (selected, next_delay)
}

fn with_registry_lock<T>(
    state_dir: &Path,
    mutate: impl FnOnce(&mut FsRegistry) -> Result<T, String>,
) -> Result<T, String> {
    let lock_path = state_dir.join("registry.lock");
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("failed to open {}: {error}", lock_path.display()))?;
    lock.lock_exclusive()
        .map_err(|error| format!("failed to lock {}: {error}", lock_path.display()))?;
    let mut registry = read_registry(state_dir)?;
    let original_generation = registry.generation;
    let result = mutate(&mut registry);
    if result.is_ok() && registry.generation != original_generation {
        write_registry(state_dir, &mut lock, &registry)?;
    }
    let _ = lock.unlock();
    result
}

fn cleanup_waiter_after_backend_error(backend: &FsLockBackend, waiter_id: Option<u64>) {
    let Some(waiter_id) = waiter_id else {
        return;
    };
    let _ = with_registry_lock(&backend.state_dir, |registry| {
        let before = registry.waiters.len();
        registry.waiters.retain(|waiter| {
            !(waiter.owner.instance_id == backend.instance_id && waiter.id == waiter_id)
        });
        if registry.waiters.len() != before {
            registry.bump();
        }
        Ok(())
    });
}

fn read_registry(state_dir: &Path) -> Result<FsRegistry, String> {
    let path = state_dir.join("registry.json");
    match File::open(&path) {
        Ok(mut file) => {
            let mut json = String::new();
            file.read_to_string(&mut json)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            if json.trim().is_empty() {
                return Ok(FsRegistry::default());
            }
            let registry: FsRegistry = serde_json::from_str(&json)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
            if registry.version != FS_REGISTRY_VERSION {
                return Err(format!(
                    "unsupported dir_lock registry version {} in {}",
                    registry.version,
                    path.display()
                ));
            }
            Ok(registry)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FsRegistry::default()),
        Err(error) => Err(format!("failed to open {}: {error}", path.display())),
    }
}

#[cfg(test)]
pub(super) fn registry_generation(state_dir: &Path) -> Result<u64, String> {
    read_registry(state_dir).map(|registry| registry.generation)
}

#[cfg(test)]
pub(super) fn registry_waiter_count(state_dir: &Path) -> Result<usize, String> {
    read_registry(state_dir).map(|registry| registry.waiters.len())
}

#[cfg(test)]
pub(super) fn set_fail_reap_for_test(state_dir: Option<PathBuf>) {
    *FAIL_REAP_FOR_TEST.lock().expect("test reap failure mutex") = state_dir;
}

fn write_registry(
    state_dir: &Path,
    lock_file: &mut File,
    registry: &FsRegistry,
) -> Result<(), String> {
    let path = state_dir.join("registry.json");
    let json = serde_json::to_vec_pretty(registry)
        .map_err(|error| format!("failed to encode dir_lock registry: {error}"))?;
    let temp_path = state_dir.join(format!(
        ".registry.json.{}.tmp",
        now_ms().saturating_add(registry.generation)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .create_new(true)
        .truncate(true)
        .open(&temp_path)
        .map_err(|error| format!("failed to open {}: {error}", temp_path.display()))?;
    file.write_all(&json)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("failed to write {}: {error}", temp_path.display()))?;
    fs::rename(&temp_path, &path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!(
            "failed to replace {} with {}: {error}",
            path.display(),
            temp_path.display()
        )
    })?;
    if let Ok(dir) = File::open(state_dir) {
        let _ = dir.sync_all();
    }
    // The registry JSON is the source of truth. The marker in `registry.lock`
    // is only a human/debugging hint, so it must not turn an already-renamed
    // successful registry update into an apparent operation failure.
    let _ = lock_file
        .seek(SeekFrom::Start(0))
        .and_then(|_| lock_file.write_all(registry.generation.to_string().as_bytes()))
        .and_then(|_| lock_file.set_len(registry.generation.to_string().len() as u64));
    Ok(())
}

pub(super) fn default_fs_state_dir() -> Result<PathBuf, String> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime).join("tau/ext-shell-dir-locks"));
    }
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
    Ok(tmp.join(format!("tau-ext-shell-dir-locks-{user}")))
}

fn ensure_private_state_dir(path: &Path, source: StateDirSource) -> Result<(), String> {
    let existed = path.exists();
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "failed to create dir_lock state dir {}: {error}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            format!(
                "failed to stat dir_lock state dir {}: {error}",
                path.display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "dir_lock state path {} is not a directory",
                path.display()
            ));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            if existed && source.reject_existing_insecure() {
                return Err(format!(
                    "dir_lock state dir {} must be private (0700), found mode {mode:o}",
                    path.display()
                ));
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
                format!(
                    "dir_lock state dir {} must be private (0700), failed to set permissions from {mode:o}: {error}",
                    path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn claim_instance_id(state_dir: &Path) -> Result<FsInstanceId, String> {
    let pid = std::process::id();
    let nanos = now_ms().saturating_mul(1_000_000);
    for attempt in 0..1000u32 {
        let instance_id = FsInstanceId::new(format!("{pid}-{nanos}-{attempt}"));
        let path = instance_lock_path(state_dir, &instance_id);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(instance_id),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to claim dir_lock instance id at {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Err("failed to claim unique dir_lock instance id".to_owned())
}

fn instance_lock_path(state_dir: &Path, instance_id: &FsInstanceId) -> PathBuf {
    state_dir
        .join("instances")
        .join(format!("{}.lock", instance_id.as_str()))
}

enum InstanceLiveness {
    Alive,
    Dead,
}

fn instance_liveness(
    state_dir: &Path,
    instance_id: &FsInstanceId,
) -> Result<InstanceLiveness, String> {
    let path = instance_lock_path(state_dir, instance_id);
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(InstanceLiveness::Dead);
        }
        Err(error) => {
            return Err(format!(
                "failed to open dir_lock instance lease {} while checking liveness: {error}",
                path.display()
            ));
        }
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(InstanceLiveness::Dead)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(InstanceLiveness::Alive),
        Err(error) => Err(format!(
            "failed to test dir_lock instance lease {}: {error}",
            path.display()
        )),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

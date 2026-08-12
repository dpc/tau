//! Per-agent remembered workdir state for one shell extension instance.

use std::collections::{HashMap, hash_map as path_std_collections_hash_map};
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One authoritative cached metadata value for an agent.
#[derive(Clone)]
enum WorkdirValue {
    /// Structurally valid absolute path text, whether currently available or
    /// stale.
    Valid(PathBuf),
    /// Present metadata that cannot safely be interpreted as an absolute path.
    Invalid,
    /// Replay failed, so absence versus durable state is unknown.
    ReplayFailed,
}

/// Atomic admission-time view used throughout one invocation.
#[derive(Clone)]
pub(crate) enum WorkdirSnapshot {
    /// Absolute committed path, including a currently stale path.
    Valid(PathBuf),
    /// Present metadata whose path representation is structurally invalid.
    Invalid,
    /// Replay failed and no operation may infer or repair state in this
    /// lifecycle.
    ReplayFailed,
}

/// One setter awaiting its matching committed metadata event.
#[derive(Clone)]
struct PendingWorkdirResult {
    /// Opaque request token that must round-trip with the committed metadata
    /// fact.
    mutation_id: tau_proto::AgentMetadataMutationId,
    /// Canonical path requested by the setter.
    expected_cwd: PathBuf,
    /// Original tool call retained until the commit boundary.
    invoke: tau_proto::ToolStarted,
    /// Lock wait metadata retained for the terminal event.
    lock_wait_duration_seconds: Option<u64>,
    /// Whether the metadata request has been emitted.
    awaiting_echo: bool,
    /// Cancellation requested after emission, terminalized only on commit.
    cancel_requested: bool,
}

/// Terminal data snapshotted at a correlated commit while the setter
/// reservation remains retained until checked terminal publication succeeds.
#[derive(Clone)]
pub(crate) struct CompletedPendingWorkdir {
    /// Original invocation retained after the metadata linearization point.
    pub(crate) invoke: tau_proto::ToolStarted,
    /// Optional directory-lock wait duration preserved for the terminal event.
    pub(crate) lock_wait_duration_seconds: Option<u64>,
    /// Whether the committed path equals the setter's requested canonical path.
    pub(crate) matched_request: bool,
    /// Whether cancellation was requested after metadata emission.
    pub(crate) cancel_requested: bool,
}

/// Cloneable, instance-scoped cache of committed per-agent workdirs and pending
/// transactions.
#[derive(Clone)]
pub(crate) struct CwdState {
    /// Configured extension instance name used only for durable metadata
    /// identity.
    instance_name: Arc<Mutex<String>>,
    /// Model-visible prefix label used only for dynamic prompt association.
    context_label: Arc<Mutex<String>>,
    /// Validated process cwd frozen after startup configuration.
    process_startup_cwd: Arc<Mutex<Result<PathBuf, String>>>,
    /// Atomic committed value cache; valid and invalid states share one lock.
    workdir_by_agent: Arc<Mutex<HashMap<tau_proto::AgentId, WorkdirValue>>>,
    /// Loaded agents waiting for initial context publication and readiness.
    pending_ready_by_agent: Arc<
        Mutex<
            HashMap<tau_proto::AgentId, (tau_proto::SessionId, tau_proto::AgentInitializationId)>,
        >,
    >,
    /// Current loaded initialization correlation retained for later context
    /// updates.
    initialization_by_agent: Arc<
        Mutex<
            HashMap<tau_proto::AgentId, (tau_proto::SessionId, tau_proto::AgentInitializationId)>,
        >,
    >,
    /// At most one pending setter transaction per agent for this instance.
    pending_workdir_by_agent: Arc<Mutex<HashMap<tau_proto::AgentId, PendingWorkdirResult>>>,
    /// Process-local monotonic source for bounded opaque mutation ids.
    next_mutation_id: Arc<AtomicU64>,
    /// Randomized per-process salt preventing mutation-token prediction.
    mutation_id_salt: u64,
}

impl CwdState {
    /// Create state with the current process cwd captured as the provisional
    /// fallback.
    pub(crate) fn new() -> Self {
        Self::with_startup_cwd(Self::read_process_startup_cwd())
    }

    /// Create state with an explicit fixture-owned fallback rather than the
    /// invoking process's working directory.
    #[cfg(any(test, feature = "echo-agent"))]
    pub(crate) fn new_with_startup_cwd(cwd: PathBuf) -> Self {
        Self::with_startup_cwd(Self::validate_startup_cwd(cwd))
    }

    fn with_startup_cwd(process_startup_cwd: Result<PathBuf, String>) -> Self {
        Self {
            instance_name: Arc::new(Mutex::new("core-shell".to_owned())),
            context_label: Arc::new(Mutex::new("default".to_owned())),
            process_startup_cwd: Arc::new(Mutex::new(process_startup_cwd)),
            workdir_by_agent: Arc::new(Mutex::new(HashMap::new())),
            pending_ready_by_agent: Arc::new(Mutex::new(HashMap::new())),
            initialization_by_agent: Arc::new(Mutex::new(HashMap::new())),
            pending_workdir_by_agent: Arc::new(Mutex::new(HashMap::new())),
            next_mutation_id: Arc::new(AtomicU64::new(1)),
            mutation_id_salt: {
                let mut hasher = path_std_collections_hash_map::RandomState::new().build_hasher();
                hasher.write_u64(0);
                hasher.finish()
            },
        }
    }

    /// Set the configured instance identity before runtime events begin.
    pub(crate) fn set_instance_name(&self, name: String) {
        *self
            .instance_name
            .lock()
            .expect("cwd instance lock poisoned") = name;
    }

    /// Set the model-visible prefix association used by prompt context.
    pub(crate) fn set_context_label(&self, prefix: Option<&tau_proto::ToolNamePrefix>) {
        *self
            .context_label
            .lock()
            .expect("workdir context label lock poisoned") =
            prefix.map_or_else(|| "default".to_owned(), |prefix| prefix.as_str().to_owned());
    }

    /// Return the prompt label for this configured instance.
    pub(crate) fn context_label(&self) -> String {
        self.context_label
            .lock()
            .expect("workdir context label lock poisoned")
            .clone()
    }

    /// Derive this instance's durable metadata key.
    pub(crate) fn key(&self) -> tau_proto::AgentMetadataKey {
        let name = self
            .instance_name
            .lock()
            .expect("cwd instance lock poisoned")
            .clone();
        tau_proto::AgentMetadataKey::new(format!("ext_{name}_cwd"))
    }

    /// Return a valid committed path, excluding missing or malformed state.
    pub(crate) fn get(&self, agent_id: &tau_proto::AgentId) -> Option<PathBuf> {
        self.workdir_by_agent
            .lock()
            .expect("workdir map lock poisoned")
            .get(agent_id)
            .and_then(|value| match value {
                WorkdirValue::Valid(path) => Some(path.clone()),
                WorkdirValue::Invalid => None,
                WorkdirValue::ReplayFailed => None,
            })
    }

    fn read_process_startup_cwd() -> Result<PathBuf, String> {
        std::env::current_dir()
            .map_err(|error| format!("failed to read ext-shell process working directory: {error}"))
            .and_then(Self::validate_startup_cwd)
    }

    fn validate_startup_cwd(cwd: PathBuf) -> Result<PathBuf, String> {
        let cwd = cwd.canonicalize().map_err(|error| {
            format!(
                "failed to canonicalize ext-shell process working directory {}: {error}",
                cwd.display()
            )
        })?;
        if !cwd.is_dir() {
            return Err(format!(
                "ext-shell process working directory is not a directory: {}",
                cwd.display()
            ));
        }
        Ok(cwd)
    }

    /// Freeze the validated process cwd after all startup cwd configuration.
    pub(crate) fn freeze_process_startup_cwd(&self) -> Result<(), String> {
        let cwd = Self::read_process_startup_cwd();
        *self
            .process_startup_cwd
            .lock()
            .expect("process startup cwd lock poisoned") = cwd.clone();
        cwd.map(|_| ())
    }

    /// Return the frozen process-startup missing-key fallback.
    pub(crate) fn process_default(&self) -> Result<PathBuf, String> {
        self.process_startup_cwd
            .lock()
            .expect("process startup cwd lock poisoned")
            .clone()
    }

    /// Atomically snapshot valid, invalid, or absent committed workdir state.
    pub(crate) fn get_or_default(&self, agent_id: &tau_proto::AgentId) -> Result<PathBuf, String> {
        match self.snapshot(agent_id)? {
            WorkdirSnapshot::Valid(path) => Ok(path),
            WorkdirSnapshot::Invalid => Err(
                "remembered workdir metadata is invalid; repair it with an absolute workdir path"
                    .to_owned(),
            ),
            WorkdirSnapshot::ReplayFailed => Err(
                "workdir replay failed for this agent; reload the agent before retrying".to_owned(),
            ),
        }
    }

    /// Atomically capture valid, invalid, or absent state for one invocation.
    pub(crate) fn snapshot(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> Result<WorkdirSnapshot, String> {
        match self
            .workdir_by_agent
            .lock()
            .expect("workdir map lock poisoned")
            .get(agent_id)
            .cloned()
        {
            Some(WorkdirValue::Valid(path)) => Ok(WorkdirSnapshot::Valid(path)),
            Some(WorkdirValue::Invalid) => Ok(WorkdirSnapshot::Invalid),
            Some(WorkdirValue::ReplayFailed) => Ok(WorkdirSnapshot::ReplayFailed),
            None => self.process_default().map(WorkdirSnapshot::Valid),
        }
    }

    /// Cache one known-valid committed absolute path.
    pub(crate) fn set(&self, agent_id: tau_proto::AgentId, cwd: PathBuf) {
        self.workdir_by_agent
            .lock()
            .expect("workdir map lock poisoned")
            .insert(agent_id, WorkdirValue::Valid(cwd));
    }

    /// Fold text metadata only when it is a structurally safe absolute path.
    ///
    /// Availability is deliberately not required: stale absolute values remain
    /// authoritative so operations fail in place rather than silently falling
    /// back.
    pub(crate) fn set_metadata_text(&self, agent_id: tau_proto::AgentId, cwd: PathBuf) -> bool {
        if cwd.as_os_str().is_empty() || !cwd.is_absolute() {
            self.set_invalid(agent_id);
            return false;
        }
        self.set(agent_id, cwd);
        true
    }

    /// Remove this instance's cached value for an unloaded agent.
    pub(crate) fn unset(&self, agent_id: &tau_proto::AgentId) {
        self.workdir_by_agent
            .lock()
            .expect("workdir map lock poisoned")
            .remove(agent_id);
    }

    /// Mark present metadata as structurally invalid without synthesizing
    /// fallback.
    pub(crate) fn set_invalid(&self, agent_id: tau_proto::AgentId) {
        self.workdir_by_agent
            .lock()
            .expect("workdir map lock poisoned")
            .insert(agent_id, WorkdirValue::Invalid);
    }

    /// Mark replay as failed so missing durable state cannot be inferred.
    pub(crate) fn set_replay_failed(&self, agent_id: tau_proto::AgentId) {
        self.workdir_by_agent
            .lock()
            .expect("workdir map lock poisoned")
            .insert(agent_id, WorkdirValue::ReplayFailed);
    }

    /// Report whether present committed metadata is structurally invalid.
    pub(crate) fn is_invalid(&self, agent_id: &tau_proto::AgentId) -> bool {
        matches!(
            self.workdir_by_agent
                .lock()
                .expect("workdir map lock poisoned")
                .get(agent_id),
            Some(WorkdirValue::Invalid)
        )
    }

    /// Report whether failed replay has latched this agent closed.
    pub(crate) fn is_replay_failed(&self, agent_id: &tau_proto::AgentId) -> bool {
        matches!(
            self.workdir_by_agent
                .lock()
                .expect("workdir map lock poisoned")
                .get(agent_id),
            Some(WorkdirValue::ReplayFailed)
        )
    }

    /// Remember an agent whose initial context waits for replay completion.
    pub(crate) fn set_pending_ready(
        &self,
        agent_id: tau_proto::AgentId,
        session_id: tau_proto::SessionId,
        agent_initialization_id: tau_proto::AgentInitializationId,
    ) {
        self.initialization_by_agent
            .lock()
            .expect("cwd initialization map lock poisoned")
            .insert(
                agent_id.clone(),
                (session_id.clone(), agent_initialization_id.clone()),
            );
        self.pending_ready_by_agent
            .lock()
            .expect("cwd ready map lock poisoned")
            .insert(agent_id, (session_id, agent_initialization_id));
    }

    /// Return the current loaded initialization correlation for context
    /// updates.
    pub(crate) fn initialization(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> Option<(tau_proto::SessionId, tau_proto::AgentInitializationId)> {
        self.initialization_by_agent
            .lock()
            .expect("cwd initialization map lock poisoned")
            .get(agent_id)
            .cloned()
    }

    /// Forget lifecycle correlation after the agent unloads.
    pub(crate) fn remove_initialization(&self, agent_id: &tau_proto::AgentId) {
        self.initialization_by_agent
            .lock()
            .expect("cwd initialization map lock poisoned")
            .remove(agent_id);
    }

    /// Consume pending context readiness after context publication.
    pub(crate) fn take_pending_ready(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> Option<(tau_proto::SessionId, tau_proto::AgentInitializationId)> {
        self.pending_ready_by_agent
            .lock()
            .expect("cwd ready map lock poisoned")
            .remove(agent_id)
    }

    /// Read pending context readiness without consuming it.
    pub(crate) fn pending_ready(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> Option<(tau_proto::SessionId, tau_proto::AgentInitializationId)> {
        self.pending_ready_by_agent
            .lock()
            .expect("cwd ready map lock poisoned")
            .get(agent_id)
            .cloned()
    }

    /// Atomically install the sole pending setter transaction for this instance
    /// and agent.
    pub(crate) fn start_pending_workdir_result(
        &self,
        agent_id: tau_proto::AgentId,
        expected_cwd: PathBuf,
        invoke: tau_proto::ToolStarted,
        lock_wait_duration_seconds: Option<u64>,
    ) -> Result<(), Box<tau_proto::ToolStarted>> {
        let mut pending = self
            .pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned");
        if pending.contains_key(&agent_id) {
            return Err(Box::new(invoke));
        }
        pending.insert(
            agent_id,
            PendingWorkdirResult {
                mutation_id: {
                    let mut hasher =
                        path_std_collections_hash_map::RandomState::new().build_hasher();
                    hasher.write_u64(self.mutation_id_salt);
                    hasher.write_u64(self.next_mutation_id.fetch_add(1, Ordering::Relaxed));
                    tau_proto::AgentMetadataMutationId::parse(format!(
                        "ext-shell-workdir-{:016x}",
                        hasher.finish()
                    ))
                    .expect("fixed-size mutation id is valid")
                },
                expected_cwd,
                invoke,
                lock_wait_duration_seconds,
                awaiting_echo: false,
                cancel_requested: false,
            },
        );
        Ok(())
    }

    /// Return the bounded correlation id reserved for an exact setter call.
    pub(crate) fn pending_workdir_mutation_id(
        &self,
        agent_id: &tau_proto::AgentId,
        call_id: &tau_proto::ToolCallId,
    ) -> Option<tau_proto::AgentMetadataMutationId> {
        self.pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned")
            .get(agent_id)
            .filter(|pending| &pending.invoke.call_id == call_id)
            .map(|pending| pending.mutation_id.clone())
    }

    /// Move an admitted setter from its non-interleavable internal reservation
    /// to awaiting its commit echo.
    pub(crate) fn mark_pending_workdir_awaiting_echo(
        &self,
        agent_id: &tau_proto::AgentId,
        call_id: &tau_proto::ToolCallId,
    ) -> bool {
        let mut pending = self
            .pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned");
        let Some(pending) = pending.get_mut(agent_id) else {
            return false;
        };
        if &pending.invoke.call_id != call_id {
            return false;
        }
        pending.awaiting_echo = true;
        true
    }

    /// Return the admission-validated canonical target reserved for a call.
    pub(crate) fn pending_workdir_target(
        &self,
        agent_id: &tau_proto::AgentId,
        call_id: &tau_proto::ToolCallId,
    ) -> Option<PathBuf> {
        self.pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned")
            .get(agent_id)
            .filter(|pending| &pending.invoke.call_id == call_id)
            .map(|pending| pending.expected_cwd.clone())
    }

    /// Snapshot a setter whose exact text-metadata echo committed, retaining
    /// its reservation until the caller confirms terminal publication.
    pub(crate) fn committed_pending_workdir_result(
        &self,
        agent_id: &tau_proto::AgentId,
        committed_cwd: &PathBuf,
        mutation_id: Option<&tau_proto::AgentMetadataMutationId>,
    ) -> Option<CompletedPendingWorkdir> {
        let pending_by_agent = self
            .pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned");
        let pending = pending_by_agent.get(agent_id)?;
        if !pending.awaiting_echo || mutation_id != Some(&pending.mutation_id) {
            return None;
        }
        Some(CompletedPendingWorkdir {
            matched_request: pending.expected_cwd == *committed_cwd,
            cancel_requested: pending.cancel_requested,
            invoke: pending.invoke.clone(),
            lock_wait_duration_seconds: pending.lock_wait_duration_seconds,
        })
    }

    /// Snapshot a correlated malformed setter echo without releasing its
    /// reservation before terminal publication succeeds.
    pub(crate) fn correlated_pending_workdir_result(
        &self,
        agent_id: &tau_proto::AgentId,
        mutation_id: Option<&tau_proto::AgentMetadataMutationId>,
    ) -> Option<CompletedPendingWorkdir> {
        let pending_by_agent = self
            .pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned");
        let pending = pending_by_agent.get(agent_id)?;
        if !pending.awaiting_echo || mutation_id != Some(&pending.mutation_id) {
            return None;
        }
        Some(CompletedPendingWorkdir {
            matched_request: false,
            cancel_requested: pending.cancel_requested,
            invoke: pending.invoke.clone(),
            lock_wait_duration_seconds: pending.lock_wait_duration_seconds,
        })
    }

    /// Consume a pending setter because it can no longer complete successfully.
    pub(crate) fn take_pending_workdir_result(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> Option<CompletedPendingWorkdir> {
        let pending = self
            .pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned")
            .remove(agent_id)?;
        Some(CompletedPendingWorkdir {
            matched_request: false,
            cancel_requested: pending.cancel_requested,
            invoke: pending.invoke,
            lock_wait_duration_seconds: pending.lock_wait_duration_seconds,
        })
    }

    /// Remove a pending setter by tool call id for cancellation.
    pub(crate) fn take_pending_workdir_by_call(
        &self,
        call_id: &tau_proto::ToolCallId,
    ) -> Option<CompletedPendingWorkdir> {
        let mut pending = self
            .pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned");
        let agent_id = pending.iter().find_map(|(agent_id, item)| {
            (&item.invoke.call_id == call_id).then(|| agent_id.clone())
        })?;
        let pending = pending.remove(&agent_id)?;
        Some(CompletedPendingWorkdir {
            matched_request: false,
            cancel_requested: pending.cancel_requested,
            invoke: pending.invoke,
            lock_wait_duration_seconds: pending.lock_wait_duration_seconds,
        })
    }

    /// Drain every setter still waiting for metadata commit during shutdown.
    pub(crate) fn take_all_pending_workdirs(&self) -> Vec<CompletedPendingWorkdir> {
        self.pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned")
            .drain()
            .map(|(_, pending)| CompletedPendingWorkdir {
                matched_request: false,
                cancel_requested: pending.cancel_requested,
                invoke: pending.invoke,
                lock_wait_duration_seconds: pending.lock_wait_duration_seconds,
            })
            .collect()
    }

    /// Mark an emitted setter for cancellation at its eventual commit boundary.
    pub(crate) fn request_pending_workdir_cancel(&self, call_id: &tau_proto::ToolCallId) -> bool {
        let mut pending = self
            .pending_workdir_by_agent
            .lock()
            .expect("workdir setter map lock poisoned");
        let Some(item) = pending
            .values_mut()
            .find(|item| &item.invoke.call_id == call_id && item.awaiting_echo)
        else {
            return false;
        };
        item.cancel_requested = true;
        true
    }
}

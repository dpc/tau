//! Per-agent remembered cwd state for the shell extension.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct PendingCdResult {
    expected_cwd: PathBuf,
    invoke: tau_proto::ToolStarted,
    lock_wait_duration_seconds: Option<u64>,
}

pub(crate) struct CompletedPendingCd {
    pub(crate) invoke: tau_proto::ToolStarted,
    pub(crate) lock_wait_duration_seconds: Option<u64>,
    pub(crate) matched_request: bool,
}

#[derive(Clone)]
pub(crate) struct CwdState {
    instance_name: Arc<Mutex<String>>,
    cwd_by_agent: Arc<Mutex<HashMap<tau_proto::AgentId, PathBuf>>>,
    pending_ready_by_agent: Arc<Mutex<HashMap<tau_proto::AgentId, tau_proto::SessionId>>>,
    pending_notice_by_agent: Arc<Mutex<HashMap<tau_proto::AgentId, PathBuf>>>,
    pending_cd_by_agent: Arc<Mutex<HashMap<tau_proto::AgentId, PendingCdResult>>>,
}

impl CwdState {
    pub(crate) fn new() -> Self {
        Self {
            instance_name: Arc::new(Mutex::new("core-shell".to_owned())),
            cwd_by_agent: Arc::new(Mutex::new(HashMap::new())),
            pending_ready_by_agent: Arc::new(Mutex::new(HashMap::new())),
            pending_notice_by_agent: Arc::new(Mutex::new(HashMap::new())),
            pending_cd_by_agent: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn set_instance_name(&self, name: String) {
        *self
            .instance_name
            .lock()
            .expect("cwd instance lock poisoned") = name;
    }

    pub(crate) fn key(&self) -> tau_proto::AgentMetadataKey {
        let name = self
            .instance_name
            .lock()
            .expect("cwd instance lock poisoned")
            .clone();
        tau_proto::AgentMetadataKey::new(format!("ext_{name}_cwd"))
    }

    pub(crate) fn get(&self, agent_id: &tau_proto::AgentId) -> Option<PathBuf> {
        self.cwd_by_agent
            .lock()
            .expect("cwd map lock poisoned")
            .get(agent_id)
            .cloned()
    }

    pub(crate) fn process_default() -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    pub(crate) fn get_or_default(&self, agent_id: &tau_proto::AgentId) -> PathBuf {
        self.get(agent_id).unwrap_or_else(Self::process_default)
    }

    pub(crate) fn set(&self, agent_id: tau_proto::AgentId, cwd: PathBuf) {
        self.cwd_by_agent
            .lock()
            .expect("cwd map lock poisoned")
            .insert(agent_id, cwd);
    }

    pub(crate) fn unset(&self, agent_id: &tau_proto::AgentId) {
        self.cwd_by_agent
            .lock()
            .expect("cwd map lock poisoned")
            .remove(agent_id);
    }

    pub(crate) fn set_pending_ready(
        &self,
        agent_id: tau_proto::AgentId,
        session_id: tau_proto::SessionId,
    ) {
        self.pending_ready_by_agent
            .lock()
            .expect("cwd ready map lock poisoned")
            .insert(agent_id, session_id);
    }

    pub(crate) fn take_pending_ready(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> Option<tau_proto::SessionId> {
        self.pending_ready_by_agent
            .lock()
            .expect("cwd ready map lock poisoned")
            .remove(agent_id)
    }

    pub(crate) fn pending_ready(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> Option<tau_proto::SessionId> {
        self.pending_ready_by_agent
            .lock()
            .expect("cwd ready map lock poisoned")
            .get(agent_id)
            .cloned()
    }

    pub(crate) fn set_pending_notice(&self, agent_id: tau_proto::AgentId, cwd: PathBuf) {
        self.pending_notice_by_agent
            .lock()
            .expect("cwd notice map lock poisoned")
            .insert(agent_id, cwd);
    }

    pub(crate) fn take_pending_notice(&self, agent_id: &tau_proto::AgentId) -> Option<PathBuf> {
        self.pending_notice_by_agent
            .lock()
            .expect("cwd notice map lock poisoned")
            .remove(agent_id)
    }

    pub(crate) fn start_pending_cd_result(
        &self,
        agent_id: tau_proto::AgentId,
        expected_cwd: PathBuf,
        invoke: tau_proto::ToolStarted,
        lock_wait_duration_seconds: Option<u64>,
    ) -> Result<(), Box<tau_proto::ToolStarted>> {
        let mut pending = self
            .pending_cd_by_agent
            .lock()
            .expect("cwd cd map lock poisoned");
        if pending.contains_key(&agent_id) {
            return Err(Box::new(invoke));
        }
        pending.insert(
            agent_id,
            PendingCdResult {
                expected_cwd,
                invoke,
                lock_wait_duration_seconds,
            },
        );
        Ok(())
    }

    pub(crate) fn take_committed_pending_cd_result(
        &self,
        agent_id: &tau_proto::AgentId,
        committed_cwd: &PathBuf,
    ) -> Option<CompletedPendingCd> {
        let pending = self
            .pending_cd_by_agent
            .lock()
            .expect("cwd cd map lock poisoned")
            .remove(agent_id)?;
        Some(CompletedPendingCd {
            matched_request: pending.expected_cwd == *committed_cwd,
            invoke: pending.invoke,
            lock_wait_duration_seconds: pending.lock_wait_duration_seconds,
        })
    }

    pub(crate) fn take_pending_cd_result(
        &self,
        agent_id: &tau_proto::AgentId,
    ) -> Option<CompletedPendingCd> {
        let pending = self
            .pending_cd_by_agent
            .lock()
            .expect("cwd cd map lock poisoned")
            .remove(agent_id)?;
        Some(CompletedPendingCd {
            matched_request: false,
            invoke: pending.invoke,
            lock_wait_duration_seconds: pending.lock_wait_duration_seconds,
        })
    }
}

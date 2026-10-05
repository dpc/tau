//! Runtime-owned history continuations; no subscription effect precedes
//! prefetch.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tau_core::{AgentStoreError, PersistenceAdmissionError};

use super::*;
use crate::history_reader::{HistoryReadCompleted, HistoryReader};

/// Aggregate accepted suffix retained by all pending history requests.
const PIN_BYTES_LIMIT: usize = 64 * 1024 * 1024;
/// Independent record-count bound, even when encoded payloads are tiny.
const PIN_RECORDS_LIMIT: u64 = 4096;

/// Read authority can disappear because of an existing writer/lifecycle
/// failure. That rejects this request without inventing a new integrity error.
fn expected_history_authority_failure(error: &PersistenceAdmissionError) -> bool {
    match error {
        PersistenceAdmissionError::Full
        | PersistenceAdmissionError::Unavailable
        | PersistenceAdmissionError::StaleLease
        | PersistenceAdmissionError::NotPrepared
        | PersistenceAdmissionError::Poisoned
        | PersistenceAdmissionError::CreationFailed => true,
        PersistenceAdmissionError::StreamNotFound | PersistenceAdmissionError::Lifecycle(_) => {
            false
        }
    }
}

/// Owns logical continuations separately from the reader's physical permits.
#[derive(Default)]
pub(crate) struct HistoryRuntime {
    /// Starts lazily on the first request requiring disk history.
    reader: Option<HistoryReader>,
    /// Monotonic process-local continuation identity.
    next_id: u64,
    /// Logical requests and their protected accepted suffixes.
    pending: BTreeMap<u64, PendingHistory>,
}

#[cfg(test)]
impl HistoryRuntime {
    /// Installs a deterministic reader cut before any request is admitted.
    pub(in crate::harness) fn set_test_reader(&mut self, reader: HistoryReader) {
        assert!(self.reader.is_none() && self.pending.is_empty());
        self.reader = Some(reader);
    }
}

/// A connection's request remains inert until exact history is available.
struct PendingHistory {
    /// Directed completion owner; disconnect cancels without waiting for I/O.
    connection: tau_proto::ConnectionId,
    /// Captured session binding, revalidated before effects.
    generation: SessionGeneration,
    /// First sequence that each request needs to retain in the live cache.
    pins: HashMap<tau_proto::AgentId, u64>,
    /// Existing synchronous operation to execute at the handoff cut.
    operation: HistoryOperation,
    /// Logical cancellation observed by the reader between physical reads.
    canceled: Arc<AtomicBool>,
}

/// Original request parameters, not a stale snapshot of membership or tree
/// state.
pub(super) enum HistoryOperation {
    /// Old subscriptions remain active while these new selectors await history.
    Subscribe {
        /// Facts selected for replay.
        historical: Vec<tau_proto::EventSelector>,
        /// Future live-delivery selectors, installed only at handoff.
        live: Vec<tau_proto::EventSelector>,
    },
    /// Render prompt anchors against current accepted history and current head.
    Tree(tau_proto::UiTreeRequest),
    /// Revalidate a prompt-anchor navigation target at handoff.
    Navigate(tau_proto::UiNavigateTree),
}

impl Drop for HistoryRuntime {
    fn drop(&mut self) {
        for pending in self.pending.values() {
            pending.canceled.store(true, Ordering::Release);
        }
    }
}

impl Harness {
    /// Uses the same validated durable membership and ephemeral overlay as
    /// replay, rather than the runtime routing registry.
    pub(super) fn history_replay_agents(
        &self,
    ) -> Result<Vec<tau_proto::AgentId>, tau_core::SessionStoreError> {
        let session = &self.session_runtime.current_session_id;
        let events = self
            .session_runtime
            .store
            .session_events(session.as_str())?;
        let mut membership = tau_core::SessionMembership::from_events(session.clone(), &events);
        let overlay = self
            .session_runtime
            .store
            .ephemeral_membership_events(session.as_str())?;
        membership.apply_ephemeral_membership_overlay(&overlay)?;
        Ok(membership.loaded_agents().into_iter().cloned().collect())
    }

    /// Starts prefetch only when a requested prepared cache is cold. A true
    /// result means the operation was deferred or explicitly rejected.
    pub(super) fn defer_history_operation(
        &mut self,
        connection: &tau_proto::ConnectionId,
        agents: Vec<tau_proto::AgentId>,
        operation: HistoryOperation,
    ) -> bool {
        if !self
            .runtime_io
            .bus
            .connections()
            .iter()
            .any(|meta| &meta.id == connection)
        {
            // Let the ordinary operation report its existing route error.
            return false;
        }
        if self
            .session_runtime
            .history
            .pending
            .values()
            .any(|pending| &pending.connection == connection)
        {
            let message = "history request already pending; retry after completion";
            if matches!(operation, HistoryOperation::Subscribe { .. }) {
                // No second uncorrelated ReplayComplete while the original
                // operation still owns its eventual completion.
                self.send_replay_error(connection, message);
            } else {
                self.fail_history_operation(connection, operation, message);
            }
            return true;
        }
        if !agents.iter().any(|id| {
            self.session_runtime
                .agent_store
                .agent_history_is_evicted(id)
        }) {
            return false;
        }
        let mut pins = HashMap::new();
        let mut prefixes = Vec::new();
        for agent in agents {
            let prefix = match self
                .session_runtime
                .agent_store
                .agent_history_prefix(&agent)
            {
                Ok(prefix) => prefix,
                Err(error) => {
                    self.fail_history_operation(connection, operation, &error.to_string());
                    self.record_history_error(error);
                    return true;
                }
            };
            if let Some(prefix) = prefix {
                pins.insert(agent, prefix.next_seq().get());
                prefixes.push(prefix);
            } else {
                match self
                    .session_runtime
                    .agent_store
                    .agent_history_pin_usage(&agent, 0)
                {
                    // No readable frontier yet: keep the prepared cache.
                    Ok(_) => {
                        pins.insert(agent, 0);
                    }
                    // Memory-only/unprepared agents have no managed projection.
                    Err(AgentStoreError::Persistence(PersistenceAdmissionError::StaleLease)) => {}
                    Err(error) => {
                        self.fail_history_operation(connection, operation, &error.to_string());
                        self.record_history_error(error);
                        return true;
                    }
                }
            }
        }
        let id = self.session_runtime.history.next_id;
        let Some(next_id) = id.checked_add(1) else {
            self.fail_history_operation(
                connection,
                operation,
                "history request identifiers exhausted",
            );
            return true;
        };
        self.session_runtime.history.next_id = next_id;
        let mut pending = PendingHistory {
            connection: connection.clone(),
            generation: self.session_runtime.current_session_generation,
            pins,
            operation,
            canceled: Arc::new(AtomicBool::new(false)),
        };
        match self.history_pins_fit(Some(&pending)) {
            Ok(true) => {}
            Ok(false) => {
                self.fail_history_operation(
                    connection,
                    pending.operation,
                    "history suffix budget exceeded; retry",
                );
                return true;
            }
            Err(error) => {
                self.fail_history_operation(connection, pending.operation, &error.to_string());
                self.record_history_error(error);
                return true;
            }
        }
        if self.session_runtime.history.reader.is_none() {
            match HistoryReader::start(self.runtime_io.tx.clone()) {
                Ok(reader) => self.session_runtime.history.reader = Some(reader),
                Err(error) => {
                    self.fail_history_operation(
                        connection,
                        pending.operation,
                        &format!("history reader unavailable: {error}"),
                    );
                    return true;
                }
            }
        }
        match self
            .session_runtime
            .history
            .reader
            .as_ref()
            .expect("reader initialized")
            .submit(id, prefixes)
        {
            Ok(canceled) => pending.canceled = canceled,
            Err(()) => {
                self.fail_history_operation(
                    connection,
                    pending.operation,
                    "history reader busy; retry",
                );
                return true;
            }
        }
        self.session_runtime.history.pending.insert(id, pending);
        true
    }

    /// Counts duplicate request pins conservatively; cold journal size itself
    /// is deliberately outside this accepted-suffix budget.
    fn history_pins_fit(&self, extra: Option<&PendingHistory>) -> Result<bool, AgentStoreError> {
        let mut bytes = 0_usize;
        let mut records = 0_u64;
        for pending in self.session_runtime.history.pending.values().chain(extra) {
            for (agent, from) in &pending.pins {
                let (pin_bytes, pin_records) = self
                    .session_runtime
                    .agent_store
                    .agent_history_pin_usage(agent, *from)?;
                bytes = bytes.saturating_add(pin_bytes);
                records = records.saturating_add(pin_records);
                if PIN_BYTES_LIMIT < bytes || PIN_RECORDS_LIMIT < records {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Enforces suffix growth at accepted append boundaries without rejecting
    /// the ordinary append. Cancellation immediately releases logical pins.
    pub(super) fn check_history_pin_budget(&mut self) {
        let message = match self.history_pins_fit(None) {
            Ok(true) => return,
            Ok(false) => "history suffix budget exceeded; retry".to_owned(),
            Err(error) => {
                let message = error.to_string();
                self.record_history_error(error);
                message
            }
        };
        let canceled = std::mem::take(&mut self.session_runtime.history.pending);
        for (_, pending) in canceled {
            pending.canceled.store(true, Ordering::Release);
            self.fail_history_operation(&pending.connection, pending.operation, &message);
        }
    }

    /// A load accepted while Subscribe is reading joins its current-roster
    /// replay. Protect the entire new resident history before load reactions
    /// can append or release their independent lifecycle protection.
    pub(super) fn pin_history_roster_agent(&mut self, agent: &tau_proto::AgentId) {
        let cold = self
            .session_runtime
            .agent_store
            .agent_history_is_evicted(agent);
        let managed = self
            .session_runtime
            .agent_store
            .has_managed_persistence_lease(agent);
        if !managed {
            return;
        }
        let mut canceled = Vec::new();
        self.session_runtime.history.pending.retain(|_, pending| {
            if !matches!(&pending.operation, HistoryOperation::Subscribe { historical, .. }
                if !historical.is_empty())
                || pending.pins.contains_key(agent)
            {
                return true;
            }
            if cold {
                pending.canceled.store(true, Ordering::Release);
                canceled.push(pending.connection.clone());
                false
            } else {
                pending.pins.insert(agent.clone(), 0);
                true
            }
        });
        for connection in canceled {
            self.send_replay_error(&connection, "history membership changed; retry");
            self.emit_session_replay_complete(
                &connection,
                Some("history membership changed; retry".to_owned()),
            );
        }
        self.check_history_pin_budget();
    }

    /// Disconnect retires the continuation immediately; physical work retains
    /// its independent permit until completion is consumed.
    pub(super) fn cancel_connection_history(&mut self, connection: &tau_proto::ConnectionId) {
        self.session_runtime.history.pending.retain(|_, pending| {
            if &pending.connection == connection {
                pending.canceled.store(true, Ordering::Release);
                false
            } else {
                true
            }
        });
    }

    /// Installs complete history and executes the original request in one
    /// runtime turn, before any other live publication can interleave.
    pub(super) fn complete_history_read(&mut self, completed: HistoryReadCompleted) {
        let pending = self.session_runtime.history.pending.remove(&completed.id);
        let histories = match completed.result {
            Ok(Some(histories)) => histories,
            Ok(None) => return,
            Err(failure) => {
                let error = failure
                    .prefix
                    .validate_authority()
                    .err()
                    .map(AgentStoreError::Persistence)
                    .unwrap_or(failure.error);
                if let Some(pending) = pending {
                    self.fail_history_operation(
                        &pending.connection,
                        pending.operation,
                        &error.to_string(),
                    );
                }
                self.record_history_error(error);
                return;
            }
        };
        let Some(pending) = pending else {
            return;
        };
        if pending.generation != self.session_runtime.current_session_generation
            || !self
                .runtime_io
                .bus
                .connections()
                .iter()
                .any(|meta| meta.id == pending.connection)
        {
            return;
        }
        for history in histories {
            if let Err(error) = self
                .session_runtime
                .agent_store
                .install_agent_history_prefix(history)
            {
                self.fail_history_operation(
                    &pending.connection,
                    pending.operation,
                    &error.to_string(),
                );
                self.record_history_error(error);
                return;
            }
        }
        match pending.operation {
            HistoryOperation::Subscribe { historical, live } => {
                let agents = match self.history_replay_agents() {
                    Ok(agents) => agents,
                    Err(error) => {
                        self.fail_history_operation(
                            &pending.connection,
                            HistoryOperation::Subscribe { historical, live },
                            &error.to_string(),
                        );
                        self.runtime_io
                            .publication
                            .pending_error
                            .get_or_insert(HarnessError::SessionStore(error));
                        return;
                    }
                };
                if agents.iter().any(|id| {
                    self.session_runtime
                        .agent_store
                        .agent_history_is_evicted(id)
                }) {
                    self.send_replay_error(
                        &pending.connection,
                        "history membership changed; retry",
                    );
                    self.emit_session_replay_complete(
                        &pending.connection,
                        Some("history membership changed; retry".to_owned()),
                    );
                    return;
                }
                let _ = self.complete_subscription_resident(&pending.connection, historical, live);
            }
            HistoryOperation::Tree(request) => {
                self.handle_ui_tree_request(&pending.connection, request)
            }
            HistoryOperation::Navigate(request) => {
                if let Err(error) = self.handle_ui_navigate_tree(&pending.connection, request) {
                    self.runtime_io
                        .publication
                        .pending_error
                        .get_or_insert(error);
                }
            }
        }
    }

    /// Expected unavailable authority is a request cancellation. Unexpected
    /// read or validation failure terminates the session through its
    /// existing error path.
    fn record_history_error(&mut self, error: AgentStoreError) {
        if !matches!(
            error,
            AgentStoreError::Persistence(ref authority) if expected_history_authority_failure(authority)
        ) {
            self.runtime_io
                .publication
                .pending_error
                .get_or_insert(HarnessError::AgentStore(error));
        }
    }

    /// Uses existing directed errors; no subscription replacement has happened.
    fn fail_history_operation(
        &mut self,
        connection: &tau_proto::ConnectionId,
        operation: HistoryOperation,
        message: &str,
    ) {
        match operation {
            HistoryOperation::Subscribe { .. } => {
                self.send_replay_error(connection, message);
                self.emit_session_replay_complete(connection, Some(message.to_owned()));
            }
            HistoryOperation::Tree(_) | HistoryOperation::Navigate(_) => {
                self.send_ui_error_response(connection, message);
            }
        }
    }
}

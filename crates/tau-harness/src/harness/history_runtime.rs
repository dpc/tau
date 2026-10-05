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
    /// Independent full-history lifecycle protection.
    pub(super) lifecycle: super::history_lifecycle::HistoryLifecycle,
    /// Starts lazily on the first request requiring disk history.
    reader: Option<HistoryReader>,
    /// Monotonic process-local continuation identity.
    next_id: u64,
    /// Logical requests and their protected accepted suffixes.
    pending: BTreeMap<u64, PendingHistory>,
}

#[cfg(test)]
impl HistoryRuntime {
    /// Exposes logical completion to test event-loop drivers, not a residency
    /// override or a substitute reader.
    pub(in crate::harness) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Installs a deterministic reader cut before any request is admitted.
    pub(in crate::harness) fn set_test_reader(&mut self, reader: HistoryReader) {
        assert!(self.reader.is_none() && self.pending.is_empty());
        self.reader = Some(reader);
    }
}

/// A connection's request remains inert until exact history is available.
struct PendingHistory {
    /// Directed completion owner; disconnect cancels without waiting for I/O.
    connection: Option<tau_proto::ConnectionId>,
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
    /// Publish a load only after its history is resident under lifecycle pins.
    Load(super::history_lifecycle::HistoryLoad),
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
    /// A late configured extension cannot activate until its exact Subscribe
    /// continuation has handed off. Other history operations do not own Ready.
    pub(super) fn extension_subscription_is_pending(
        &self,
        connection: &tau_proto::ConnectionId,
    ) -> bool {
        self.session_runtime
            .history
            .pending
            .values()
            .any(|pending| {
                pending.connection.as_ref() == Some(connection)
                    && matches!(pending.operation, HistoryOperation::Subscribe { .. })
            })
    }

    /// Terminal startup-subscription failure isolates this exact late
    /// connection. Initial startup and already-online subscription updates keep
    /// their existing behavior; neither retries nor read deadlines are added.
    pub(super) fn fail_late_extension_subscription(
        &mut self,
        connection: &tau_proto::ConnectionId,
        message: &str,
    ) {
        if self.runtime_io.publication.pending_error.is_none()
            && self.extensions.initial_tool_preflight_complete
            && self.session_initialized(&self.session_runtime.current_session_id)
            && self
                .extensions
                .entries
                .get(connection)
                .is_some_and(|entry| entry.state == ExtensionState::Handshaking)
            && let Err(error) = self.handle_extension_protocol_failure(
                connection,
                format!("startup history subscription failed: {message}"),
            )
        {
            self.runtime_io
                .publication
                .pending_error
                .get_or_insert(error);
        }
    }

    /// Only the actual successful resident handoff may release a received
    /// Ready. Failed replay is not success merely because routing returned
    /// `Ok`.
    pub(super) fn finish_late_extension_subscription(
        &mut self,
        connection: &tau_proto::ConnectionId,
    ) {
        if self.runtime_io.publication.pending_error.is_some()
            || !self.extensions.initial_tool_preflight_complete
            || !self.session_initialized(&self.session_runtime.current_session_id)
            || !self
                .extensions
                .entries
                .get(connection)
                .is_some_and(|entry| entry.state == ExtensionState::Handshaking)
        {
            return;
        }
        let activated = self.maybe_finish_extension_activation(Some(connection));
        self.finish_late_subscription_activation(activated);
    }

    /// Completes the post-activation callback boundary. Activation may already
    /// have made the extension Ready before deferred operational work fails.
    pub(super) fn finish_late_subscription_activation(
        &mut self,
        activated: Result<(), HarnessError>,
    ) {
        let result = activated.and_then(|()| {
            if self.runtime_io.publication.pending_error.is_some() {
                return Ok(());
            }
            self.drain_pending_tool_invocations()
        });
        if let Err(error) = result {
            self.runtime_io
                .publication
                .pending_error
                .get_or_insert(error);
            return;
        }
        if self.runtime_io.publication.pending_error.is_none() {
            self.try_advance_queue();
        }
    }

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
            .any(|pending| pending.connection.as_ref() == Some(connection))
        {
            let message = "history request already pending; retry after completion";
            if matches!(operation, HistoryOperation::Subscribe { .. }) {
                // No second uncorrelated ReplayComplete while the original
                // operation still owns its eventual completion.
                self.send_replay_error(connection, message);
            } else {
                self.fail_history_operation(Some(connection), operation, message);
            }
            return true;
        }
        self.defer_owned_history_operation(Some(connection), agents, operation)
    }

    /// Shares reader admission and suffix accounting with lifecycle-owned
    /// loads.
    pub(super) fn defer_owned_history_operation(
        &mut self,
        connection: Option<&tau_proto::ConnectionId>,
        agents: Vec<tau_proto::AgentId>,
        operation: HistoryOperation,
    ) -> bool {
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
                    self.fail_history_operation_with_error(connection, operation, error);
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
                        self.fail_history_operation_with_error(connection, operation, error);
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
            connection: connection.cloned(),
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
                self.fail_history_operation_with_error(connection, pending.operation, error);
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
            self.fail_history_operation(pending.connection.as_ref(), pending.operation, &message);
        }
        self.evict_eligible_history();
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
                canceled.extend(pending.connection.clone());
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
            self.fail_late_extension_subscription(&connection, "history membership changed; retry");
        }
        self.check_history_pin_budget();
    }

    /// Disconnect retires the continuation immediately; physical work retains
    /// its independent permit until completion is consumed.
    pub(super) fn cancel_connection_history(&mut self, connection: &tau_proto::ConnectionId) {
        self.session_runtime.history.pending.retain(|_, pending| {
            if pending.connection.as_ref() == Some(connection) {
                pending.canceled.store(true, Ordering::Release);
                false
            } else {
                true
            }
        });
        self.evict_eligible_history();
    }

    /// Installs complete history and executes the original request in one
    /// runtime turn, before any other live publication can interleave.
    pub(super) fn complete_history_read(&mut self, completed: HistoryReadCompleted) {
        let agents = self
            .session_runtime
            .history
            .pending
            .get(&completed.id)
            .map(|pending| pending.pins.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for agent in &agents {
            self.begin_history_preparation(agent);
        }
        self.complete_history_read_inner(completed);
        for agent in agents {
            self.finish_history_preparation(&agent);
        }
    }

    /// Keep the handoff indivisible; eviction runs only after its consumers.
    fn complete_history_read_inner(&mut self, completed: HistoryReadCompleted) {
        let pending = self.session_runtime.history.pending.remove(&completed.id);
        let histories = match completed.result {
            Ok(Some(histories)) => histories,
            Ok(None) => {
                if let Some(pending) = pending {
                    self.fail_history_operation(
                        pending.connection.as_ref(),
                        pending.operation,
                        "history read canceled",
                    );
                }
                return;
            }
            Err(failure) => {
                let error = failure
                    .prefix
                    .validate_authority()
                    .err()
                    .map(AgentStoreError::Persistence)
                    .unwrap_or(failure.error);
                if let Some(pending) = pending {
                    self.fail_history_operation_with_error(
                        pending.connection.as_ref(),
                        pending.operation,
                        error,
                    );
                } else {
                    self.record_history_error(error);
                }
                return;
            }
        };
        let Some(pending) = pending else {
            return;
        };
        if pending.generation != self.session_runtime.current_session_generation
            || pending.connection.as_ref().is_some_and(|connection| {
                !self
                    .runtime_io
                    .bus
                    .connections()
                    .iter()
                    .any(|meta| &meta.id == connection)
            })
        {
            // Do not hand off stale authority, or leave a received Ready able
            // to activate merely because its pending entry was removed.
            if let Some(connection) = pending.connection.as_ref()
                && matches!(pending.operation, HistoryOperation::Subscribe { .. })
            {
                self.fail_late_extension_subscription(
                    connection,
                    "history session binding changed",
                );
            }
            return;
        }
        for history in histories {
            if let Err(error) = self
                .session_runtime
                .agent_store
                .install_agent_history_prefix(history)
            {
                self.fail_history_operation_with_error(
                    pending.connection.as_ref(),
                    pending.operation,
                    error,
                );
                return;
            }
        }
        match pending.operation {
            HistoryOperation::Load(load) => self.publish_history_ready_load(load),
            HistoryOperation::Subscribe { historical, live } => {
                let connection = pending
                    .connection
                    .as_ref()
                    .expect("Subscribe has a connection");
                let agents = match self.history_replay_agents() {
                    Ok(agents) => agents,
                    Err(error) => {
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
                    self.send_replay_error(connection, "history membership changed; retry");
                    self.emit_session_replay_complete(
                        connection,
                        Some("history membership changed; retry".to_owned()),
                    );
                    self.fail_late_extension_subscription(
                        connection,
                        "history membership changed; retry",
                    );
                    return;
                }
                let _ = self.complete_subscription_resident(connection, historical, live);
            }
            HistoryOperation::Tree(request) => self.handle_ui_tree_request(
                pending.connection.as_ref().expect("Tree has a connection"),
                request,
            ),
            HistoryOperation::Navigate(request) => {
                if let Err(error) = self.handle_ui_navigate_tree(
                    pending
                        .connection
                        .as_ref()
                        .expect("Navigate has a connection"),
                    request,
                ) {
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
    pub(super) fn record_history_error(&mut self, error: AgentStoreError) {
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

    /// Classifies typed failures before considering request teardown. Fatal
    /// session errors must not disconnect a held extension and thereby release
    /// queued work while the runtime is still returning to its fatal boundary.
    pub(super) fn fail_history_operation_with_error(
        &mut self,
        connection: Option<&tau_proto::ConnectionId>,
        operation: HistoryOperation,
        error: AgentStoreError,
    ) {
        let message = error.to_string();
        self.record_history_error(error);
        self.fail_history_operation(connection, operation, &message);
    }

    /// Uses existing directed errors; no subscription replacement has happened.
    fn fail_history_operation(
        &mut self,
        connection: Option<&tau_proto::ConnectionId>,
        operation: HistoryOperation,
        message: &str,
    ) {
        if self.runtime_io.publication.pending_error.is_some() {
            // Session termination owns cleanup. Even setting the fatal error
            // first is insufficient if disconnect then advances queues.
            return;
        }
        match operation {
            HistoryOperation::Load(load) => self.fail_history_load(&load, message),
            HistoryOperation::Subscribe { .. } => {
                let connection = connection.expect("Subscribe has a connection");
                self.send_replay_error(connection, message);
                self.emit_session_replay_complete(connection, Some(message.to_owned()));
                self.fail_late_extension_subscription(connection, message);
            }
            HistoryOperation::Tree(_) | HistoryOperation::Navigate(_) => {
                let connection = connection.expect("UI request has a connection");
                self.send_ui_error_response(connection, message);
            }
        }
    }

    /// Returns the earliest retained request cut for one managed agent.
    pub(super) fn history_request_pin(&self, agent: &tau_proto::AgentId) -> Option<u64> {
        self.session_runtime
            .history
            .pending
            .values()
            .filter_map(|pending| pending.pins.get(agent).copied())
            .min()
    }

    /// Teardown cancels only the exact agent's load continuations.
    pub(super) fn cancel_agent_history_load(&mut self, agent: &tau_proto::AgentId) {
        self.session_runtime.history.pending.retain(|_, pending| {
            if matches!(&pending.operation, HistoryOperation::Load(load) if &load.loaded.agent_id == agent) {
                pending.canceled.store(true, Ordering::Release);
                false
            } else {
                true
            }
        });
    }
}

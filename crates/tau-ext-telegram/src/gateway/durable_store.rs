//! Serialized access to the Telegram gateway's single durable state file.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::{Arc, Barrier};

use super::{GatewayDurableState, GatewayRegistrationKey, GatewaySaveCut, GatewayStateSaveError};
use crate::live_checkpoint::{TelegramReportId, TelegramUpdateId};

/// Shared transactional owner of one gateway stream's durable state.
pub(super) struct GatewayDurableStore {
    /// Existing per-stream state path; `None` is used only by isolated unit
    /// tests that never exercise persistence.
    path: Option<PathBuf>,
    /// Canonical in-memory state serialized with every file transaction.
    state: Mutex<GatewayDurableState>,
    /// Whether an installed-but-not-directory-synced save made recovery
    /// ambiguous and disabled further operation.
    poisoned: AtomicBool,
    /// Deterministic next save cut for persistence regression tests.
    #[cfg(test)]
    next_save_cut: Mutex<GatewaySaveCut>,
    /// One-shot pause after the first health check and before lock acquisition.
    #[cfg(test)]
    pause_after_initial_health_check: Mutex<Option<GatewayStorePause>>,
    /// One-shot pause after the locked health check in an ACK transaction.
    #[cfg(test)]
    pause_after_locked_ack_health_check: Mutex<Option<GatewayStorePause>>,
}

/// Deterministic two-party pause used by store concurrency tests.
#[cfg(test)]
#[derive(Clone)]
pub(super) struct GatewayStorePause {
    /// Signals that the instrumented transaction reached the cut.
    pub(super) entered: Arc<Barrier>,
    /// Releases the instrumented transaction from the cut.
    pub(super) resume: Arc<Barrier>,
}

impl GatewayDurableStore {
    /// Create a durable store backed by the existing per-stream state file.
    pub(super) fn new(path: PathBuf, state: GatewayDurableState) -> Self {
        Self {
            path: Some(path),
            state: Mutex::new(state),
            poisoned: AtomicBool::new(false),
            #[cfg(test)]
            next_save_cut: Mutex::new(GatewaySaveCut::None),
            #[cfg(test)]
            pause_after_initial_health_check: Mutex::new(None),
            #[cfg(test)]
            pause_after_locked_ack_health_check: Mutex::new(None),
        }
    }

    /// Create an in-memory store for socket tests unrelated to persistence.
    #[cfg(test)]
    pub(super) fn in_memory(state: GatewayDurableState) -> Self {
        Self {
            path: None,
            state: Mutex::new(state),
            poisoned: AtomicBool::new(false),
            next_save_cut: Mutex::new(GatewaySaveCut::None),
            pause_after_initial_health_check: Mutex::new(None),
            pause_after_locked_ack_health_check: Mutex::new(None),
        }
    }

    /// Clone the latest committed state without retaining the transaction lock.
    pub(super) fn snapshot(&self) -> Result<GatewayDurableState, String> {
        self.ensure_healthy()?;
        self.pause_after_initial_health_check();
        let state = self.state.lock().expect("durable state lock");
        self.ensure_healthy()?;
        Ok(state.clone())
    }

    /// Commit one locally processed update while retaining any ACK that a
    /// socket thread committed after processing began.
    pub(super) fn commit_processed_update(
        &self,
        candidate: &GatewayDurableState,
        update_id: TelegramUpdateId,
    ) -> Result<GatewayDurableState, String> {
        self.ensure_healthy()?;
        self.pause_after_initial_health_check();
        let mut state = self.state.lock().expect("durable state lock");
        self.ensure_healthy()?;
        let before = state.clone();

        state.linked_chat = candidate.linked_chat;
        state.recent_update_ids = candidate.recent_update_ids.clone();
        state.processed_update_count = candidate.processed_update_count;
        state.rejected_update_count = candidate.rejected_update_count;
        state.selected_route = candidate.selected_route.clone();
        state
            .checkpoints
            .merge_update_from(&candidate.checkpoints, update_id);
        let next_update_offset = state.next_update_offset;
        state.next_update_offset = state.checkpoints.advance_prefix(next_update_offset);

        if let Err(error) = self.save(&state) {
            return self.handle_save_error(&mut state, before, error);
        }
        Ok(state.clone())
    }

    /// Atomically commit one exact canonical ACK and its bounded retry
    /// authorization before returning success.
    pub(super) fn acknowledge_delivery(
        &self,
        report_id: &TelegramReportId,
        route: &GatewayRegistrationKey,
    ) -> Result<GatewayDurableState, String> {
        self.ensure_healthy()?;
        self.pause_after_initial_health_check();
        let mut state = self.state.lock().expect("durable state lock");
        self.ensure_healthy()?;
        self.pause_after_locked_ack_health_check();
        if let Some(acknowledgement) = state
            .recent_acknowledgements
            .iter()
            .find(|acknowledgement| acknowledgement.report_id == *report_id)
        {
            return if acknowledgement.route == *route {
                Ok(state.clone())
            } else {
                Err("Telegram gateway report does not belong to this sidecar.".to_owned())
            };
        }

        let Some(delivery) = state.checkpoints.pending_delivery(report_id) else {
            return Err("Telegram gateway report is not pending.".to_owned());
        };
        if route.session_id != delivery.session_id || route.agent_id.as_str() != delivery.agent_id {
            return Err("Telegram gateway report does not belong to this sidecar.".to_owned());
        }

        let before = state.clone();
        state.remember_acknowledgement(report_id.clone(), route.clone());
        let acknowledged = state.checkpoints.acknowledge(report_id);
        // ast-grep-ignore: debug-assert-expression-must-not-mutate
        debug_assert!(acknowledged, "pending delivery must be acknowledgeable");
        let next_update_offset = state.next_update_offset;
        state.next_update_offset = state.checkpoints.advance_prefix(next_update_offset);
        if let Err(error) = self.save(&state) {
            return self.handle_save_error(&mut state, before, error);
        }
        Ok(state.clone())
    }

    /// Persist one state through the existing atomic state-file boundary.
    fn save(&self, state: &GatewayDurableState) -> Result<(), GatewayStateSaveError> {
        let cut = {
            #[cfg(test)]
            {
                std::mem::replace(
                    &mut *self.next_save_cut.lock().expect("save cut lock"),
                    GatewaySaveCut::None,
                )
            }
            #[cfg(not(test))]
            {
                GatewaySaveCut::None
            }
        };
        self.path
            .as_deref()
            .map_or(Ok(()), |path| state.save_with_cut(path, cut))
    }

    /// Roll back a pre-install failure or poison the store after an ambiguous
    /// installed-state failure.
    fn handle_save_error(
        &self,
        state: &mut GatewayDurableState,
        before: GatewayDurableState,
        error: GatewayStateSaveError,
    ) -> Result<GatewayDurableState, String> {
        if error.installed {
            self.poisoned.store(true, Ordering::SeqCst);
            return Err(format!(
                "{error}; Telegram gateway durable state is commit-unknown and the gateway must restart"
            ));
        }
        *state = before;
        Err(error.to_string())
    }

    /// Refuse all operations after an installed state could not be made
    /// durably unambiguous.
    fn ensure_healthy(&self) -> Result<(), String> {
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(
                "Telegram gateway durable state is commit-unknown; restart required".to_owned(),
            );
        }
        Ok(())
    }

    /// Cut the next state-file transaction at one deterministic boundary.
    #[cfg(test)]
    pub(super) fn fail_next_save_at(&self, cut: GatewaySaveCut) {
        *self.next_save_cut.lock().expect("save cut lock") = cut;
    }

    /// Pause the next store operation after its initial unlocked health check.
    #[cfg(test)]
    pub(super) fn pause_next_after_initial_health_check(&self, pause: GatewayStorePause) {
        *self
            .pause_after_initial_health_check
            .lock()
            .expect("initial health pause lock") = Some(pause);
    }

    /// Pause the next ACK after it acquires the state lock and rechecks health.
    #[cfg(test)]
    pub(super) fn pause_next_ack_after_locked_health_check(&self, pause: GatewayStorePause) {
        *self
            .pause_after_locked_ack_health_check
            .lock()
            .expect("locked ACK health pause lock") = Some(pause);
    }

    /// Enter and consume the configured initial-health-check pause.
    #[cfg(test)]
    fn pause_after_initial_health_check(&self) {
        if let Some(pause) = self
            .pause_after_initial_health_check
            .lock()
            .expect("initial health pause lock")
            .take()
        {
            pause.entered.wait();
            pause.resume.wait();
        }
    }

    /// Enter and consume the configured locked ACK health-check pause.
    #[cfg(test)]
    fn pause_after_locked_ack_health_check(&self) {
        if let Some(pause) = self
            .pause_after_locked_ack_health_check
            .lock()
            .expect("locked ACK health pause lock")
            .take()
        {
            pause.entered.wait();
            pause.resume.wait();
        }
    }

    /// No-op initial-health pause in production builds.
    #[cfg(not(test))]
    fn pause_after_initial_health_check(&self) {}

    /// No-op locked-ACK pause in production builds.
    #[cfg(not(test))]
    fn pause_after_locked_ack_health_check(&self) {}
}

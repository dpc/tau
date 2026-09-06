//! Backend-only non-generating warm observations, not refresh lifecycle
//! authority.

use std::sync::Arc;

use serde_json::json;
use tau_provider::cache_diagnostic::Reservation;

use super::{
    CacheAttempt, DispatchEvidence, RequestShape, dispatch_fields, merge, response_fields,
};
use crate::common::{LlmError, StreamState};
use crate::responses::ResponsesConfig;

/// Closed facts from one entered backend warm invocation and its existing
/// repair.
pub(crate) struct WarmObservation {
    /// Metadata-only attempt; never creates a prompt identity.
    attempt: Arc<CacheAttempt>,
    /// Whether the existing pool branch consumed its immediate repair.
    repair_used: bool,
    /// Sticky parsed semantic progress, including a discarded first dispatch.
    semantic_progress: bool,
    /// Closed pool-selected lifecycle fact for the next dispatch.
    connection_state: &'static str,
}

impl WarmObservation {
    /// Apply the existing durability permission and immutable metadata
    /// selection.
    pub(crate) fn new(
        request: &crate::Prompt<'_>,
        refresh_id: Option<&tau_proto::ProviderCacheRefreshId>,
        enabled: bool,
    ) -> Option<Self> {
        Some(Self {
            attempt: Arc::new(CacheAttempt::warm(request, refresh_id, enabled)?),
            repair_used: false,
            semantic_progress: false,
            connection_state: "unknown",
        })
    }

    /// Observe only the pool's actual initial socket choice.
    pub(crate) fn socket(&mut self, reused: bool) {
        self.connection_state = if reused { "reused" } else { "new" };
    }

    /// Record the existing recoverable-error branch before replacement upgrade.
    pub(crate) fn repair(&mut self) {
        self.repair_used = true;
        self.connection_state = "replaced";
    }

    /// Retain parsed progress without retaining raw data or changing retry
    /// policy.
    pub(crate) fn progress(&mut self, parsed: bool) {
        self.semantic_progress |= parsed;
    }

    /// Prepare only closed scalar facts from the already-lowered warm envelope.
    pub(crate) fn dispatch(
        &self,
        config: &ResponsesConfig,
        request: &crate::Prompt<'_>,
        shape: RequestShape,
    ) -> DispatchEvidence {
        DispatchEvidence {
            attempt: Arc::clone(&self.attempt),
            fields: dispatch_fields(
                config,
                request,
                shape,
                self.repair_used,
                if self.repair_used {
                    "other_typed"
                } else {
                    "none"
                },
                self.connection_state,
            ),
        }
    }

    /// Observe the backend result after socket publication, before the caller
    /// may override its status for a worker deadline or reject a stale
    /// terminal.
    pub(crate) fn finish(
        &self,
        config: &ResponsesConfig,
        result: &Result<Option<StreamState>, LlmError>,
    ) {
        let Some(reservation) = Reservation::acquire() else {
            return;
        };
        let count = self.attempt.dispatch_count();
        let success = matches!(result, Ok(Some(_)));
        let canceled = matches!(result, Err(LlmError::Canceled));
        let state = result.as_ref().ok().and_then(Option::as_ref);
        let mut record = self.attempt.common(&reservation, "attempt_end");
        merge(
            &mut record,
            response_fields(
                config,
                state.and_then(|s| s.provider_terminal_event.as_ref()),
            ),
        );
        merge(
            &mut record,
            json!({
                "dispatch_count": count,
                "successful_dispatch_index": success.then_some(count).filter(|n| *n > 0),
                "outcome": if success { "success" } else if canceled { "canceled" }
                    else if count == 0 { "pre_dispatch_failure" } else { "error" },
                "failure_class": result.as_ref().err().and_then(|e| e.failure_kind()),
                "semantic_progress": self.semantic_progress,
                "repair_used": self.repair_used,
                "reconnect_count": u64::from(self.repair_used),
                "chain_strip_count": null
            }),
        );
        self.attempt.submit(record, reservation);
    }
}

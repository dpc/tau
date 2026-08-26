//! Central ingress, event delivery, and diagnostic I/O ownership.

use super::*;

/// Runtime I/O state with deliberate field order for shutdown and drop.
pub(crate) struct RuntimeIoState {
    /// Sender side of the central harness event channel.
    pub(crate) tx: Sender<HarnessEvent>,
    /// Receiver side of the central harness event channel.
    pub(crate) rx: Receiver<HarnessEvent>,
    /// Producer side of the bounded component-ingress lane.
    pub(crate) component_ingress_tx: ComponentIngressSender,
    /// Harness-owned component-ingress slot, closed before producer joins.
    pub(crate) component_ingress: ComponentIngress,
    /// Event held while overdue-deadline catch-up completes.
    pub(crate) pending_runtime_event: Option<HarnessEvent>,
    /// Deterministic post-receive clock cut for scheduler tests.
    #[cfg(test)]
    pub(crate) runtime_event_receive_cut: Option<Instant>,
    /// Live connection event bus and cursor owner.
    pub(crate) bus: EventBus,
    /// Runtime event sequencer.
    pub(crate) event_log: std::sync::Arc<EventLog>,
    /// Harness notices replayed to late UI clients.
    pub(crate) replayable_harness_notices: Vec<tau_proto::HarnessNotice>,
    /// Last diagnostic warning about a lagging live follower.
    pub(crate) last_live_egress_lag_warning: Option<Instant>,
    /// Producer for the best-effort debug event log.
    pub(crate) debug_log: Option<DebugEventLog>,
    /// Test-visible synchronous debug writer rollback poison.
    pub(crate) debug_log_poisoned: bool,
    /// Interception, deferred publication, and continuation state.
    pub(crate) publication: PublicationState,
}

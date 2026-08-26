//! Owns publication interception and deferred commit continuations.
//!
//! Publication preserves one serial enqueue, intercept, commit, and post-commit
//! pipeline. Disconnect settlement remains in this owner because it resumes only
//! after the complete synthesized terminal batch commits.

use super::*;

/// Runtime state for the harness publication pipeline.
///
/// The event log remains outside this state because it owns durable sequencing,
/// while this state owns only transient work surrounding each commit.
#[derive(Default)]
pub(crate) struct PublicationState {
    /// Source inherited by synchronous successors of a committed event/report.
    pub(crate) derived_source: Option<ConnectionId>,
    /// Event emission interceptors, exact name first and prefix fallback.
    pub(crate) interceptors: InterceptorRegistry,
    /// Interceptor connections awaiting one stale uncorrelated reply.
    pub(crate) suspended_interceptor_connections: HashSet<tau_proto::ConnectionId>,
    /// Currently in-flight interception.
    pub(crate) pending_intercept: Option<PendingIntercept>,
    /// Fatal error raised while a parked publish commits downstream.
    pub(crate) pending_error: Option<HarnessError>,
    /// Foreground terminals synthesized as one disconnect batch.
    pub(crate) disconnect_terminal_batch_pending: HashSet<ToolCallId>,
    /// Calls whose runtime settlement waits for the whole disconnect batch.
    pub(crate) disconnect_terminal_batch_completed: Vec<(ToolCallId, AgentId)>,
    /// Publishes deferred behind the currently in-flight interception.
    pub(crate) deferred: VecDeque<DeferredPublish>,
    /// Publish-idle dispatch and committed activation obligations.
    pub(crate) idle_dispatches: VecDeque<interception::DeferredPromptDispatch>,
}

//! Runtime ownership for canonical standalone backend-attempt accounting.

use super::interception::OwnedPublication;
use super::*;

/// Durable publication phase for one logical accounting identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum StandaloneAccountingPublicationPhase {
    /// Initial request-counting observation.
    Initial,
    /// Sole final correction for a canceled terminal.
    Correction,
}

/// Independent retention identity for one accounting publication.
pub(crate) type StandaloneAccountingPublicationKey = (
    tau_proto::AgentPromptId,
    tau_proto::ProviderAttempt,
    StandaloneAccountingPublicationPhase,
);

/// Ledger phase already folded for one prompt/attempt identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FoldedStandaloneAccountingPhase {
    /// Initial observation committed and cannot be corrected.
    Final,
    /// Cancellation-time observation awaits a possible live correction.
    AwaitingCorrection,
    /// The sole correction committed.
    Corrected,
}

/// Correction held until its required initial observation commits.
#[derive(Clone)]
pub(crate) struct PendingStandaloneAccountingCorrection {
    /// Runtime agent route used for canonical publication.
    pub(crate) cid: AgentId,
    /// Final correction DTO.
    pub(crate) corrected: tau_proto::ProviderStandaloneExecutionAccountingCorrected,
}

/// Immutable correlation captured when a standalone prompt reaches its
/// provider.
#[derive(Clone)]
pub(crate) struct StandaloneExecutionAccountingOwner {
    /// Session whose ledger owns every attempt for this prompt.
    pub(crate) session_id: tau_proto::SessionId,
    /// Durable agent whose journal owns the accounting facts.
    pub(crate) agent_id: tau_proto::AgentId,
    /// Runtime agent route used for live publication.
    pub(crate) cid: AgentId,
    /// Standalone transaction that dispatched the prompt.
    pub(crate) transaction_id: tau_proto::CompactionTransactionId,
    /// Provider-qualified model captured by the transaction.
    pub(crate) model: tau_proto::ModelId,
    /// Effective rates captured from the exact provider route.
    pub(crate) estimated_cost_rates: tau_proto::EstimatedApiCostRates,
}

/// Exact accounting publication retained independently from compaction
/// outcomes.
#[derive(Clone)]
pub(crate) struct RetainedStandaloneAccountingPublication {
    /// Runtime agent route used to retry the approved fact.
    pub(crate) cid: AgentId,
    /// Interceptor-approved event and semantic parent.
    pub(crate) publication: OwnedPublication,
    /// Canonical harness source preserved from the initial publication.
    pub(crate) source: Option<tau_proto::ConnectionId>,
}

/// Process-local state derived from durable standalone accounting facts.
#[derive(Default)]
pub(crate) struct StandaloneExecutionAccountingState {
    /// Dispatch owners preserved through prompt cancellation and terminal
    /// cleanup.
    pub(crate) owners: HashMap<tau_proto::AgentPromptId, StandaloneExecutionAccountingOwner>,
    /// Attempt keys already published or awaiting an exact retained retry.
    pub(crate) observed_attempts: HashSet<(tau_proto::AgentPromptId, tau_proto::ProviderAttempt)>,
    /// Highest retry attempt reported for each standalone prompt.
    pub(crate) highest_retry_attempt: HashMap<tau_proto::AgentPromptId, u32>,
    /// Prompts whose out-of-contract retry count already emitted one warning.
    pub(crate) rejected_retry_bounds: HashSet<tau_proto::AgentPromptId>,
    /// Frozen cancellation-time terminal attempt for each live correction
    /// owner.
    pub(crate) awaiting_corrections: HashMap<tau_proto::AgentPromptId, tau_proto::ProviderAttempt>,
    /// Corrections waiting for their initial observation to commit.
    pub(crate) pending_corrections: HashMap<
        (tau_proto::AgentPromptId, tau_proto::ProviderAttempt),
        PendingStandaloneAccountingCorrection,
    >,
    /// Agent endpoints whose unload waits for final accounting commit.
    pub(crate) pending_agent_removals: HashSet<AgentId>,
    /// Correction publications already queued for this process.
    pub(crate) observed_corrections:
        HashSet<(tau_proto::AgentPromptId, tau_proto::ProviderAttempt)>,
    /// Append-rejected facts keyed independently from compaction publications.
    pub(crate) retained:
        HashMap<StandaloneAccountingPublicationKey, RetainedStandaloneAccountingPublication>,
    /// Committed phases already folded into this run's live ledger.
    pub(crate) folded: HashMap<
        (tau_proto::AgentPromptId, tau_proto::ProviderAttempt),
        FoldedStandaloneAccountingPhase,
    >,
    /// Restored costs waiting for a runtime route and creator topology.
    pub(crate) pending_costs: HashMap<tau_proto::AgentId, tau_proto::EstimatedApiCost>,
}

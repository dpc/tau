use serde::Serialize;
use tau_proto::AgentId;

/// One explicit gap caused by unavailable or incomplete journal evidence.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MissingAccountingData {
    /// Agent whose journal contains the gap.
    pub agent_id: AgentId,
    /// Durable field or lifecycle fact that was unavailable.
    pub fact: MissingAccountingFact,
}

/// Closed classification of unavailable accounting authority.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum MissingAccountingFact {
    /// Creation predates creator provenance.
    #[serde(rename = "agent.started.creator")]
    AgentStartedCreator,
    /// Prompt predates outer-turn ownership.
    #[serde(rename = "agent.prompt_started.outer_turn_id")]
    PromptOuterTurnId,
    /// Prompt predates captured model parameters.
    #[serde(rename = "agent.prompt_started.model_params")]
    PromptModelParams,
    /// Response lacks captured cost authority.
    #[serde(rename = "provider.response_finished.estimated_api_cost")]
    ResponseEstimatedCost,
    /// Membership references an absent agent journal.
    #[serde(rename = "agent.journal.missing")]
    AgentJournalMissing,
}

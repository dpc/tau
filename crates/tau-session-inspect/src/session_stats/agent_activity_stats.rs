use serde::Serialize;
use tau_proto::{AgentCreator, AgentId};

use super::{ActivityCounts, ModelNativeReasoningEffortStats, ToolActivityStats};

/// Exact journal-derived activity for one session member.
#[derive(Clone, Debug, Serialize)]
pub struct AgentActivityStats {
    /// Durable public agent identity.
    pub agent_id: AgentId,
    /// Role captured at creation.
    pub role: Option<String>,
    /// Latest durable human-friendly name.
    pub name: Option<String>,
    /// Tagged authenticated creator provenance.
    pub creator: Option<AgentCreator>,
    /// Exact aggregate for this agent.
    pub totals: ActivityCounts,
    /// Breakdowns by captured provider model and effort.
    pub models: Vec<ModelNativeReasoningEffortStats>,
    /// Tool counts grouped by stable tool name.
    pub tools: Vec<ToolActivityStats>,
}

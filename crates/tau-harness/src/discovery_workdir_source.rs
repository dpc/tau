//! Retained inputs and correlation for one configured workdir discovery source.

use std::time::Instant;

use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};

/// One loaded agent's source-local binding and replaceable refresh owner.
#[derive(Clone)]
pub(crate) struct DiscoveryWorkdirSource {
    /// Canonical metadata namespace owned by this configured source.
    pub(crate) metadata_key: tau_proto::AgentMetadataKey,
    /// Stable user-only candidates used when the project becomes unavailable.
    pub(crate) user_skills: Vec<(tau_proto::SkillName, DiscoveredSkill)>,
    /// Complete user identities and metadata, including collision losers.
    pub(crate) user_candidates: Vec<(tau_proto::SkillName, DiscoveredSkill)>,
    /// Original source-owned user state, restored by replacement scanners.
    pub(crate) retained_user_state: Vec<u8>,
    /// Stable user-only instructions used when the project becomes unavailable.
    pub(crate) user_agents_files: Vec<DiscoveredAgentsFile>,
    /// Latest source-local refresh and its eventual installed disposition.
    pub(crate) refresh: Option<tau_proto::DiscoveryRefreshOutcome>,
    /// Scan deadline, cleared on accepted reply or degraded fallback.
    pub(crate) deadline: Option<Instant>,
    /// Current source degradation, retained until a successful replacement.
    pub(crate) error: Option<String>,
}

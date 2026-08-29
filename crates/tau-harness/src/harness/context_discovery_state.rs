//! Owns skill, AGENTS.md, prompt-context, and template discovery projections.
//!
//! This state keeps mutable readiness attempts beside the frozen snapshots and
//! rendered previews whose freshness depends on them.

use super::*;

/// Runtime state for session and per-agent context discovery.
pub(crate) struct ContextDiscoveryState {
    /// Selected skill winners keyed by skill name.
    pub(crate) skills: HashMap<tau_proto::SkillName, DiscoveredSkill>,
    /// All discovered candidates retained behind each selected winner.
    pub(crate) skill_candidates: HashMap<tau_proto::SkillName, Vec<DiscoveredSkill>>,
    /// AGENTS.md files in extension delivery order.
    pub(crate) agents_files: Vec<DiscoveredAgentsFile>,
    /// Session-scoped JSON context contributions from extensions.
    pub(crate) agent_context: AgentContextStore,
    /// Registered per-agent prompt-context providers.
    pub(crate) agent_context_providers: HashSet<tau_proto::ConnectionId>,
    /// Registered session-wide prompt-context providers.
    pub(crate) session_context_providers: HashSet<tau_proto::ConnectionId>,
    /// Mutable readiness state for current agent load attempts.
    pub(crate) pending_agents: HashMap<tau_proto::AgentId, PendingAgentDiscovery>,
    /// Frozen effective discovery snapshots for initialized agents.
    pub(crate) frozen_agents: HashMap<tau_proto::AgentId, FrozenAgentDiscovery>,
    /// Canonical initialized context projection for each loaded agent.
    pub(crate) initialized_agent_context:
        HashMap<tau_proto::AgentId, tau_proto::HarnessAgentContextInitialized>,
    /// Prompt previews waiting for ordinary context initialization.
    pub(super) pending_rendered_prompts: HashMap<tau_proto::AgentId, PendingRenderedPreview>,
    /// Current canonical full-session skill projection.
    pub(crate) session_skills: tau_proto::HarnessSessionSkillsAvailable,
    /// Extension prompt fragments keyed by source and fragment name.
    pub(crate) prompt_fragments:
        BTreeMap<tau_proto::ConnectionId, BTreeMap<String, PromptFragment>>,
    /// Loaded system-prompt templates keyed by template name.
    pub(crate) system_prompt_templates: HashMap<String, String>,
    /// Reusable configured renderer and exact source parse cache.
    pub(crate) prompt_template_engine: crate::prompt::PromptTemplateEngine,
    /// Sessions whose AGENTS.md and skill discovery completed.
    pub(crate) initialized_sessions: HashSet<SessionId>,
}

impl ContextDiscoveryState {
    /// Creates the initial built-in discovery projection for one session.
    pub(crate) fn new(
        session_id: SessionId,
        system_prompt_templates: HashMap<String, String>,
    ) -> Self {
        let skills = built_in_discovered_skills();
        let session_skills = tau_proto::HarnessSessionSkillsAvailable {
            session_id,
            skills: effective_skills(&skills),
        };
        let skill_candidates = skills
            .iter()
            .map(|(name, skill)| (name.clone(), vec![skill.clone()]))
            .collect();
        Self {
            skills,
            skill_candidates,
            agents_files: Vec::new(),
            agent_context: AgentContextStore::default(),
            agent_context_providers: HashSet::new(),
            session_context_providers: HashSet::new(),
            pending_agents: HashMap::new(),
            frozen_agents: HashMap::new(),
            initialized_agent_context: HashMap::new(),
            pending_rendered_prompts: HashMap::new(),
            session_skills,
            prompt_fragments: BTreeMap::new(),
            system_prompt_templates,
            prompt_template_engine: PromptTemplateEngine::default(),
            initialized_sessions: HashSet::new(),
        }
    }
}

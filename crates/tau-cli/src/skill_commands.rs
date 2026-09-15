//! Client-side canonical session skill state for completion and summaries.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::locked;

/// Shared skill snapshot used by the renderer and prompt completer.
#[derive(Clone, Debug, Default)]
pub(crate) struct SkillCommandState {
    /// Canonical session snapshot shared by completion and initialization UI.
    inner: Arc<Mutex<SkillSnapshot>>,
}

/// Derived client-side views of one canonical session skill snapshot.
#[derive(Debug, Default)]
struct SkillSnapshot {
    /// Role-neutral baseline for prospective-agent completion.
    session_skills: Vec<tau_proto::DiscoveryEffectiveSkill>,
    /// Session owning the cached agent projections.
    session_id: Option<tau_proto::SessionId>,
    /// Frozen eligible skills, including unadvertised user-invocable skills.
    agent_skills: BTreeMap<tau_proto::AgentId, Vec<tau_proto::DiscoveryEffectiveSkill>>,
    /// Selected existing agent; absence selects prospective-role completion.
    selected_agent: Option<tau_proto::AgentId>,
    /// Effective role and configured group for the next new agent.
    prospective_role: (String, String),
    /// Every canonical skill name available in the session.
    available_names: BTreeSet<tau_proto::SkillName>,
}

/// Menu metadata derived only from an eligible skill.
#[derive(Clone, Debug)]
struct SkillCompletion {
    /// Human-readable skill purpose.
    description: String,
    /// Optional invocation syntax.
    argument_hint: Option<String>,
    /// File or built-in origin displayed to the user.
    source_label: String,
}

impl SkillCommandState {
    /// Create an empty skill-command state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Replace all derived skill state from one canonical session snapshot.
    pub(crate) fn apply_session_snapshot(
        &self,
        snapshot: &tau_proto::HarnessSessionSkillsAvailable,
    ) {
        let mut inner = locked(&self.inner);
        if inner.session_id.as_ref() != Some(&snapshot.session_id) {
            inner.agent_skills.clear();
        }
        inner.session_id = Some(snapshot.session_id.clone());
        inner.session_skills = snapshot.skills.clone();
        inner.available_names.clear();
        for skill in &snapshot.skills {
            inner.available_names.insert(skill.name.clone());
        }
    }

    /// Replace an agent's frozen completion projection even while it is hidden.
    pub(crate) fn apply_agent_snapshot(
        &self,
        snapshot: &tau_proto::HarnessAgentContextInitialized,
    ) {
        let mut inner = locked(&self.inner);
        if inner
            .session_id
            .as_ref()
            .is_some_and(|session| session != &snapshot.session_id)
        {
            return;
        }
        inner.session_id = Some(snapshot.session_id.clone());
        inner
            .agent_skills
            .insert(snapshot.agent_id.clone(), snapshot.effective_skills.clone());
    }

    /// Select an existing agent without falling back to the session inventory.
    pub(crate) fn select_agent(&self, agent: Option<tau_proto::AgentId>) {
        locked(&self.inner).selected_agent = agent;
    }

    /// Set the effective role/group used only when composing for a new agent.
    pub(crate) fn set_prospective_role(&self, role: String, group: String) {
        locked(&self.inner).prospective_role = (role, group);
    }

    /// Count available session skills not advertised in one agent's prompt.
    pub(crate) fn unadvertised_count(
        &self,
        advertised: &[tau_proto::DiscoveryEffectiveSkill],
    ) -> usize {
        let advertised = advertised
            .iter()
            .map(|skill| skill.name.clone())
            .collect::<BTreeSet<_>>();
        locked(&self.inner)
            .available_names
            .difference(&advertised)
            .count()
    }

    /// Build the `:skill` argument completer for the current snapshot.
    pub(crate) fn arg_completer(&self) -> tau_cli_term::ArgCompleter {
        let state = self.clone();
        Arc::new(move |args| state.complete_args(args))
    }

    fn complete_args(&self, args: &[&str]) -> Vec<tau_cli_term::CompletionItem> {
        if args.len() != 1 {
            return Vec::new();
        }
        let needle = args[0].to_lowercase();
        let mut prefix_matches = Vec::new();
        let mut substring_matches = Vec::new();
        let inner = locked(&self.inner);
        let skills = match &inner.selected_agent {
            Some(agent) => inner
                .agent_skills
                .get(agent)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            None => &inner.session_skills,
        };
        let mut eligible = skills
            .iter()
            .filter(|skill| {
                skill.user_invocable
                    && (inner.selected_agent.is_some()
                        || skill
                            .visibility
                            .allows(&inner.prospective_role.0, &inner.prospective_role.1))
            })
            .collect::<Vec<_>>();
        eligible.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in eligible {
            let name = skill.name.as_str();
            let source_label = match &skill.source {
                tau_proto::DiscoveryEffectiveSkillSource::File { path } => {
                    path.display().to_string()
                }
                tau_proto::DiscoveryEffectiveSkillSource::BuiltIn => "built-in skill".to_owned(),
            };
            let skill = SkillCompletion {
                description: skill.description.clone(),
                argument_hint: skill.argument_hint.clone(),
                source_label,
            };
            let lower_name = name.to_lowercase();
            let item = tau_cli_term::CompletionItem::new(name, skill.menu_description());
            if needle.is_empty() || lower_name.starts_with(&needle) {
                prefix_matches.push(item);
            } else if lower_name.contains(&needle) {
                substring_matches.push(item);
            }
        }
        prefix_matches.extend(substring_matches);
        prefix_matches
    }
}

impl SkillCompletion {
    fn menu_description(&self) -> String {
        let mut description = self.description.clone();
        if let Some(hint) = self
            .argument_hint
            .as_deref()
            .filter(|hint| !hint.is_empty())
        {
            description.push_str(" — ");
            description.push_str(hint);
        }
        description.push_str(" (");
        description.push_str(&self.source_label);
        description.push(')');
        description
    }
}

#[cfg(test)]
mod tests;

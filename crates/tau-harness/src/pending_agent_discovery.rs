//! Mutable discovery state for one correlated agent initialization.

use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};

/// Mutable discovery state isolated to one correlated agent initialization.
#[derive(Clone)]
pub(crate) struct PendingAgentDiscovery {
    /// Revision advanced by each canonical mutation or admitted initial input.
    pub(crate) revision: u64,
    /// Latest revision already submitted for durable installation.
    pub(crate) publishing_revision: Option<u64>,
    /// Exact interceptor-approved install rejected by definite capacity
    /// pressure.
    pub(crate) retained_install: Option<tau_proto::AgentInitializationContextSet>,
    /// Setters superseded before installation, acknowledged as errors with the
    /// next installed view rather than abandoned with a held reservation.
    pub(crate) superseded_refreshes: Vec<tau_proto::DiscoveryRefreshOutcome>,
    /// Sampled loadability retained for unchanged user and other-source inputs.
    pub(crate) validated_skills:
        std::collections::HashMap<(tau_proto::ConnectionId, tau_proto::SkillName, String), bool>,
    /// Source-local workdir bindings with retained user-only fallbacks.
    pub(crate) workdir_sources: std::collections::HashMap<
        tau_proto::ConnectionId,
        crate::discovery_workdir_source::DiscoveryWorkdirSource,
    >,
    /// Exact load attempt that owns this state.
    pub(crate) initialization_id: tau_proto::AgentInitializationId,
    /// Candidate sets seeded from the session baseline and replaced per source.
    pub(crate) skill_candidates:
        std::collections::HashMap<tau_proto::SkillName, Vec<DiscoveredSkill>>,
    /// Effective winners derived atomically from `skill_candidates`.
    pub(crate) skills: std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    /// Raw ordered AGENTS.md inputs, retained before role filtering.
    pub(crate) agents_files: Vec<DiscoveredAgentsFile>,
    /// Captured providers that have not acknowledged this initialization.
    pub(crate) waiting_on: std::collections::HashSet<tau_proto::ConnectionId>,
}

impl PendingAgentDiscovery {
    /// Transfer one retained configured source to its authoritative replacement
    /// connection without rescanning user inputs or changing load identity.
    pub(crate) fn rebind_source(
        &mut self,
        previous: &tau_proto::ConnectionId,
        replacement: &tau_proto::ConnectionId,
    ) {
        let Some(mut source) = self.workdir_sources.remove(previous) else {
            return;
        };
        for (_, skill) in source
            .user_skills
            .iter_mut()
            .chain(&mut source.user_candidates)
        {
            skill.source_id = replacement.clone();
        }
        for file in &mut source.user_agents_files {
            file.source_id = replacement.clone();
        }
        self.workdir_sources.insert(replacement.clone(), source);
        for skill in self
            .skill_candidates
            .values_mut()
            .flatten()
            .chain(self.skills.values_mut())
        {
            if &skill.source_id == previous {
                skill.source_id = replacement.clone();
            }
        }
        for file in &mut self.agents_files {
            if &file.source_id == previous {
                file.source_id = replacement.clone();
            }
        }
        self.validated_skills = self
            .validated_skills
            .drain()
            .map(|((source, name, path), valid)| {
                (
                    (
                        if &source == previous {
                            replacement.clone()
                        } else {
                            source
                        },
                        name,
                        path,
                    ),
                    valid,
                )
            })
            .collect();
        if self.waiting_on.remove(previous) {
            self.waiting_on.insert(replacement.clone());
        }
    }
}

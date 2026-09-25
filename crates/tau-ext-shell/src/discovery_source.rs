//! Shell-host discovery with independently captured user and project scopes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tau_proto::{DiscoveryAgentsFile, DiscoverySkillCandidate};
use tau_skills::{SkillDir, SkillDiscovery};

use crate::agents::{ancestor_agents_roots, discover_agents_files_from_roots, user_agents_roots};
use crate::{
    DiscoveryScan, discovery_skill_candidate, project_skill_dirs, push_skill_diagnostic_requests,
    session_skill_dirs,
};

#[cfg(test)]
mod tests;

/// One shell source's stable user contribution and explicit project scan
/// inputs.
pub(crate) struct DiscoverySource {
    /// User instructions and candidates captured once for this source
    /// lifecycle.
    user: CapturedScope,
    /// Home boundary used to exclude user roots from project skill discovery.
    home: Option<PathBuf>,
}

/// Parsed candidates plus wire metadata sampled on the shell's execution host.
struct CapturedScope {
    /// Complete ordered candidates needed for cross-scope name resolution.
    skills: SkillDiscovery,
    /// Canonical source paths and timestamps captured even for hidden
    /// candidates.
    wire_skills: HashMap<PathBuf, DiscoverySkillCandidate>,
    /// Ordered instruction contents sampled from this scope's roots.
    agents_files: Vec<DiscoveryAgentsFile>,
}

impl CapturedScope {
    /// Scan one scope without prematurely discarding skill collision
    /// candidates.
    fn scan(skill_dirs: &[SkillDir], agents_roots: Vec<PathBuf>) -> Self {
        let skills = SkillDiscovery::scan(skill_dirs);
        let wire_skills = skills
            .candidates()
            .map(|(skill, modified)| {
                (
                    skill.file_path.clone(),
                    discovery_skill_candidate(skill.clone(), modified),
                )
            })
            .collect();
        let agents_files = discover_agents_files_from_roots(agents_roots)
            .into_iter()
            .map(|file| DiscoveryAgentsFile {
                file_path: file.file_path,
                content: file.content,
            })
            .collect();
        Self {
            skills,
            wire_skills,
            agents_files,
        }
    }
}

impl DiscoverySource {
    /// Capture user roots once, preserving their existing discovery lifecycle.
    pub(crate) fn new(home: Option<PathBuf>) -> Self {
        let user = CapturedScope::scan(
            &session_skill_dirs(None, home.clone()),
            home.as_deref().map(user_agents_roots).unwrap_or_default(),
        );
        Self { user, home }
    }

    /// Replace project inputs from an explicit workdir, without rereading
    /// users.
    ///
    /// An absent project scope produces user-only discovery. Callers own
    /// workdir availability validation and reporting; this scanner
    /// preserves existing optional-file skip rules.
    pub(crate) fn scan_project(
        &self,
        session_id: tau_proto::SessionId,
        cwd: Option<&Path>,
    ) -> DiscoveryScan {
        let project_dirs = project_skill_dirs(cwd, self.home.as_deref());
        let project = CapturedScope::scan(
            &project_dirs,
            cwd.map(ancestor_agents_roots).unwrap_or_default(),
        );
        let resolved = SkillDiscovery::resolve([&project.skills, &self.user.skills]);
        let skills = resolved
            .skills
            .into_iter()
            .map(|skill| {
                project
                    .wire_skills
                    .get(&skill.file_path)
                    .or_else(|| self.user.wire_skills.get(&skill.file_path))
                    .expect("resolved skill belongs to a captured scope")
                    .clone()
            })
            .collect();
        let mut seen = HashSet::new();
        let agents_files = self
            .user
            .agents_files
            .iter()
            .chain(&project.agents_files)
            .filter(|file| seen.insert(file.file_path.clone()))
            .cloned()
            .collect();
        let (malformed, ordinary): (Vec<_>, Vec<_>) = resolved
            .diagnostics
            .into_iter()
            .partition(|diagnostic| diagnostic.kind == tau_skills::DiagnosticKind::Frontmatter);
        let frontmatter_diagnostics = malformed
            .into_iter()
            .map(|diagnostic| tau_proto::DiscoveryFrontmatterDiagnostic {
                file_path: diagnostic.path,
                message: diagnostic.message,
            })
            .collect();
        let mut diagnostics = Vec::new();
        push_skill_diagnostic_requests(&mut diagnostics, ordinary);
        DiscoveryScan {
            snapshot: tau_proto::ExtensionSessionDiscoverySnapshotDeclared {
                frontmatter_diagnostics,
                session_id,
                skills,
                agents_files,
            },
            diagnostics,
        }
    }
}

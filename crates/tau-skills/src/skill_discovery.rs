//! Retained discovery inputs for replacing one scope without rescanning
//! another.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::time::SystemTime;

use crate::{
    DEFAULT_DISCOVERY_LIMITS, DiagnosticKind, DiscoveryLimits, LoadSkillsResult, Skill,
    SkillDiagnostic, SkillDir, collision_message, compare_skill_candidate,
    discover_skill_paths_with_limits, load_skill_from_content, read_skill_discovery_content,
    skill_modified_time,
};

/// Ordered, sampled discovery inputs, including candidates hidden by
/// collisions.
///
/// Keep independently scoped scans (for example project and user roots)
/// separate and resolve them together in original root order. Resolving each
/// scope first loses candidates: precedence applies only when both roots
/// specify it, so the collision comparator is not transitive.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SkillDiscovery {
    /// Scan observations in traversal order, before name collision resolution.
    entries: Vec<DiscoveryEntry>,
}

/// One ordered observation, preserving diagnostics alongside candidate
/// sampling.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum DiscoveryEntry {
    /// A candidate whose parsed metadata and ordering inputs are frozen.
    Candidate(SampledSkill),
    /// A warning or skip emitted at this position during the scan.
    Diagnostic(SkillDiagnostic),
}

/// One parsed candidate and the exact collision inputs sampled with it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SampledSkill {
    /// Parsed metadata, including the root's advertisement default.
    skill: Skill,
    /// Modification time sampled during discovery, not during resolution.
    #[serde(with = "sampled_time")]
    modified: Option<SystemTime>,
    /// Explicit root priority, considered only against another explicit
    /// priority.
    source_precedence: Option<u32>,
}

/// Lossless signed timestamps, including files dated before the Unix epoch.
mod sampled_time;

impl SkillDiscovery {
    /// Scan roots in order with the same traversal and read limits as skill
    /// loading.
    ///
    /// No name collisions are discarded. This captures metadata, not live skill
    /// bodies; subsequent resolution does no filesystem I/O.
    pub fn scan(dirs: &[SkillDir]) -> Self {
        Self::scan_with_limits(dirs, DEFAULT_DISCOVERY_LIMITS)
    }

    /// Iterate every candidate with its discovery-time modification timestamp.
    ///
    /// Consumers can capture host-specific source metadata for losing
    /// candidates too, without rereading it when a different scope is
    /// replaced.
    pub fn candidates(&self) -> impl Iterator<Item = (&Skill, Option<SystemTime>)> {
        self.entries.iter().filter_map(|entry| match entry {
            DiscoveryEntry::Candidate(candidate) => Some((&candidate.skill, candidate.modified)),
            DiscoveryEntry::Diagnostic(_) => None,
        })
    }

    /// Capture bounded traversal observations before resolving name collisions.
    pub(crate) fn scan_with_limits(dirs: &[SkillDir], limits: DiscoveryLimits) -> Self {
        let mut entries = Vec::new();
        for dir in dirs {
            let (paths, diagnostics) = discover_skill_paths_with_limits(&dir.path, limits);
            entries.extend(diagnostics.into_iter().map(DiscoveryEntry::Diagnostic));
            for path in paths {
                let content = match read_skill_discovery_content(&path) {
                    Ok(content) => content,
                    Err(diagnostic) => {
                        entries.push(DiscoveryEntry::Diagnostic(diagnostic));
                        continue;
                    }
                };
                let (skill, diagnostics) = load_skill_from_content(&content, &path);
                entries.extend(diagnostics.into_iter().map(DiscoveryEntry::Diagnostic));
                if let Some(mut skill) = skill {
                    if !skill.add_to_prompt_explicit {
                        skill.add_to_prompt |= dir.add_to_prompt_by_default;
                    }
                    entries.push(DiscoveryEntry::Candidate(SampledSkill {
                        modified: skill_modified_time(&skill.file_path),
                        source_precedence: dir.source_precedence,
                        skill,
                    }));
                }
            }
        }
        Self { entries }
    }

    /// Resolve ordered scans using sampled root precedence and modification
    /// times.
    ///
    /// Supply scans in original root order, not separately resolved winners.
    /// Ties retain the first candidate; output skills are sorted by name.
    /// Diagnostics retain their original positions relative to collision
    /// notices. Neither on-disk edits nor removal of a captured source
    /// alter this result.
    pub fn resolve<'a>(scans: impl IntoIterator<Item = &'a Self>) -> LoadSkillsResult {
        let mut winners = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for entry in scans.into_iter().flat_map(|scan| &scan.entries) {
            match entry {
                DiscoveryEntry::Diagnostic(diagnostic) => diagnostics.push(diagnostic.clone()),
                DiscoveryEntry::Candidate(candidate) => {
                    let winner = match winners.entry(candidate.skill.name.clone()) {
                        Entry::Vacant(entry) => {
                            entry.insert(candidate);
                            continue;
                        }
                        Entry::Occupied(entry) => entry.into_mut(),
                    };
                    let ordering = compare_skill_candidate(
                        candidate.source_precedence,
                        candidate.modified,
                        winner.source_precedence,
                        winner.modified,
                    );
                    let reason = if candidate.source_precedence != winner.source_precedence
                        && candidate.source_precedence.is_some()
                        && winner.source_precedence.is_some()
                    {
                        "higher-priority skill root"
                    } else if ordering == Ordering::Equal {
                        "same or unavailable modified time"
                    } else {
                        "newer modified time"
                    };
                    let ignored = if ordering == Ordering::Greater {
                        std::mem::replace(winner, candidate)
                    } else {
                        candidate
                    };
                    diagnostics.push(SkillDiagnostic {
                        path: ignored.skill.file_path.clone(),
                        kind: DiagnosticKind::Collision,
                        message: collision_message(
                            &candidate.skill.name,
                            &winner.skill.file_path,
                            &ignored.skill.file_path,
                            reason,
                        ),
                    });
                }
            }
        }
        LoadSkillsResult {
            skills: winners
                .into_values()
                .map(|candidate| candidate.skill.clone())
                .collect(),
            diagnostics,
        }
    }
}

#[cfg(test)]
mod tests;

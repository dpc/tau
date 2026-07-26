//! Wire types for atomic discovery declarations and canonical projections.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{AgentId, SessionId, SkillName};

/// Opaque correlation for one attempt to initialize a loaded agent's context.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentInitializationId(String);

impl AgentInitializationId {
    /// Construct an initialization correlation from a harness-minted value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the opaque initialization correlation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A signed skill-file modification time in microseconds from the Unix epoch.
///
/// Negative values represent timestamps before `1970-01-01T00:00:00Z`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiscoveryModifiedMicros(i64);

impl DiscoveryModifiedMicros {
    /// Construct a sampled timestamp from signed Unix-epoch microseconds.
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// Return signed Unix-epoch microseconds.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// One raw skill candidate in a complete extension discovery snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoverySkillCandidate {
    /// Declared skill name.
    pub name: SkillName,
    /// Human-readable skill description.
    pub description: String,
    /// Absolute path to the skill file.
    pub file_path: PathBuf,
    /// Whether the skill should appear in the model's system prompt.
    pub add_to_prompt: bool,
    /// Whether users may explicitly invoke the skill with `:skill`.
    pub user_invocable: bool,
    /// Whether model-side skill discovery and loading should hide the skill.
    pub disable_model_invocation: bool,
    /// Optional UI hint for arguments accepted by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// File modification time sampled by the discovery owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_modified: Option<DiscoveryModifiedMicros>,
}

/// One ordered AGENTS.md file in a complete extension discovery snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryAgentsFile {
    /// Absolute path to the AGENTS.md file.
    pub file_path: PathBuf,
    /// Complete bounded file contents sampled by the discovery owner.
    pub content: String,
}

/// Reconstructible source of one validated effective skill.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiscoveryEffectiveSkillSource {
    /// A skill loaded from an absolute Markdown file path.
    File {
        /// Absolute path from which the skill can be loaded.
        path: PathBuf,
    },
    /// A Tau built-in skill resolved by its enclosing effective skill name.
    BuiltIn,
}

/// One validated effective skill in a harness-owned discovery projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryEffectiveSkill {
    /// Effective skill name after validation and collision resolution.
    pub name: SkillName,
    /// Human-readable skill description.
    pub description: String,
    /// Reconstructible effective skill source.
    pub source: DiscoveryEffectiveSkillSource,
    /// Whether the skill appears in the model's system prompt.
    pub add_to_prompt: bool,
    /// Whether users may explicitly invoke the skill with `:skill`.
    pub user_invocable: bool,
    /// Whether model-side skill discovery and loading hide the skill.
    pub disable_model_invocation: bool,
    /// Optional UI hint for arguments accepted by the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

/// Bounded metadata for one AGENTS.md file used during agent initialization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryAgentsFileSummary {
    /// Absolute path to the AGENTS.md file.
    pub file_path: PathBuf,
    /// Number of logical text lines in the sampled contents.
    pub lines: u64,
    /// Number of bytes in the sampled contents.
    pub bytes: u64,
}

/// An extension's complete session-baseline discovery contribution.
///
/// The transient declaration atomically replaces its source; empty lists clear
/// it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionSessionDiscoverySnapshotDeclared {
    /// Session to which this complete source snapshot belongs.
    pub session_id: SessionId,
    /// Complete skill candidate list for this source.
    pub skills: Vec<DiscoverySkillCandidate>,
    /// Complete ordered AGENTS.md file list for this source.
    pub agents_files: Vec<DiscoveryAgentsFile>,
}

/// An extension's complete contribution for one agent initialization.
///
/// The transient declaration atomically replaces its pending source; empty
/// lists clear it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionAgentDiscoverySnapshotDeclared {
    /// Session containing the target agent.
    pub session_id: SessionId,
    /// Agent receiving this discovery snapshot.
    pub agent_id: AgentId,
    /// Exact initialization attempt to which this snapshot belongs.
    pub agent_initialization_id: AgentInitializationId,
    /// Complete skill candidate list for this source and initialization.
    pub skills: Vec<DiscoverySkillCandidate>,
    /// Complete ordered AGENTS.md file list for this source and initialization.
    pub agents_files: Vec<DiscoveryAgentsFile>,
}

/// Durable replacement state for one completed agent initialization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentInitializationContextSet {
    /// Session containing the initialized agent.
    pub session_id: SessionId,
    /// Agent whose bootstrap context and skill state are replaced.
    pub agent_id: AgentId,
    /// Exact initialization attempt that produced this state.
    pub agent_initialization_id: AgentInitializationId,
    /// Rendered user-role bootstrap instructions, or `None` to clear them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_message: Option<String>,
    /// Complete effective skill state frozen for this agent.
    pub effective_skills: Vec<DiscoveryEffectiveSkill>,
    /// Ordered AGENTS.md summaries represented by `agents_message`.
    pub agents_files: Vec<DiscoveryAgentsFileSummary>,
}

/// Harness-owned current projection of one completed agent initialization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessAgentContextInitialized {
    /// Session containing the initialized agent.
    pub session_id: SessionId,
    /// Agent whose current initialization is projected.
    pub agent_id: AgentId,
    /// Exact initialization attempt represented by this projection.
    pub agent_initialization_id: AgentInitializationId,
    /// Exact effective skills listed in this agent's system prompt.
    pub listed_skills: Vec<DiscoveryEffectiveSkill>,
    /// Exact ordered AGENTS.md files used for this agent's bootstrap block.
    pub agents_files: Vec<DiscoveryAgentsFileSummary>,
}

/// Harness-owned complete current session skill projection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessSessionSkillsAvailable {
    /// Session represented by this full replacement snapshot.
    pub session_id: SessionId,
    /// Complete validated, collision-resolved session skill state.
    pub skills: Vec<DiscoveryEffectiveSkill>,
}

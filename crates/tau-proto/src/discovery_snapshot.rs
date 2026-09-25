//! Wire types for atomic discovery declarations and canonical projections.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{AgentId, SessionId, SkillName};

crate::validated_string_newtype!(
    /// Opaque correlation for one attempt to initialize a loaded agent's
    /// context.
    AgentInitializationId,
    AgentInitializationIdParseError,
    "agent initialization id",
    64
);

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
    /// Role policy sampled with this candidate; omission is unrestricted.
    #[serde(default)]
    pub visibility: crate::ContextVisibility,
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
    /// Sampled role policy retained across projection and initialization
    /// replay.
    #[serde(default)]
    pub visibility: crate::ContextVisibility,
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
    /// Malformed context headers encountered by this scan, requiring UI alerts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frontmatter_diagnostics: Vec<DiscoveryFrontmatterDiagnostic>,
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
    /// Source-local workdir binding declared during initial agent discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir_binding: Option<DiscoveryWorkdirBinding>,
    /// Exact harness-requested refresh, absent for initial discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_id: Option<u64>,
    /// Recoverable discovery failure; the snapshot contains retained user
    /// inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_error: Option<String>,
    /// Malformed context headers encountered by this correlated scan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frontmatter_diagnostics: Vec<DiscoveryFrontmatterDiagnostic>,
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
    /// Runtime revision of this load's replaceable discovery state.
    #[serde(default)]
    pub discovery_revision: u64,
    /// Source refreshes represented by this installed replacement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovery_refreshes: Vec<DiscoveryRefreshOutcome>,
    /// Explicit degraded context diagnostics retained with the current view.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovery_diagnostics: Vec<String>,
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
    /// Runtime revision of this load's installed discovery state.
    #[serde(default)]
    pub discovery_revision: u64,
    /// Correlated refresh outcomes acknowledged only after durable
    /// installation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovery_refreshes: Vec<DiscoveryRefreshOutcome>,
    /// Explicit degraded context diagnostics in the installed view.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovery_diagnostics: Vec<String>,
    /// Session containing the initialized agent.
    pub session_id: SessionId,
    /// Agent whose current initialization is projected.
    pub agent_id: AgentId,
    /// Exact initialization attempt represented by this projection.
    pub agent_initialization_id: AgentInitializationId,
    /// Exact effective skills listed in this agent's system prompt.
    pub listed_skills: Vec<DiscoveryEffectiveSkill>,
    /// Complete frozen eligible set for selected-agent completion. Older
    /// projections omit this field; consumers must not fall back to session
    /// inventory for an initialized agent.
    #[serde(default)]
    pub effective_skills: Vec<DiscoveryEffectiveSkill>,
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

/// A recoverable malformed context header reported with its source snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryFrontmatterDiagnostic {
    /// Absolute source path, including files whose skill identity was unusable.
    pub file_path: PathBuf,
    /// Actionable parser diagnostic; never a replacement for the source body.
    pub message: String,
}

/// One configured source's persistent-workdir discovery binding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryWorkdirBinding {
    /// Metadata key whose canonical changes replace this source's project
    /// inputs.
    pub metadata_key: crate::AgentMetadataKey,
    /// Captured user candidates retained when project discovery is unavailable.
    pub user_skills: Vec<DiscoverySkillCandidate>,
    /// Complete sampled user candidates, before source-local collisions, whose
    /// eligibility remains fixed for this load.
    pub user_candidates: Vec<DiscoverySkillCandidate>,
    /// Source-owned serialized user scope, echoed unchanged on refresh so a
    /// replacement process need not recapture user files.
    pub retained_user_state: Vec<u8>,
    /// Captured user instructions retained when project discovery is
    /// unavailable.
    pub user_agents_files: Vec<DiscoveryAgentsFile>,
}

/// Harness request to scan one source after its canonical workdir changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessAgentDiscoveryRefreshRequested {
    /// Session owning this transient request.
    pub session_id: SessionId,
    /// Loaded agent whose source contribution must be replaced.
    pub agent_id: AgentId,
    /// Stable load identity; refreshing does not reinitialize other providers.
    pub agent_initialization_id: AgentInitializationId,
    /// Exact source-local metadata key selected by the initial binding.
    pub metadata_key: crate::AgentMetadataKey,
    /// Actual committed value, absent after an unset rather than a startup
    /// default.
    pub metadata_value: Option<crate::CborValue>,
    /// Unique runtime correlation, superseded by a later canonical mutation.
    pub refresh_id: u64,
    /// Original source-owned user scope from this load's accepted binding.
    pub retained_user_state: Vec<u8>,
}

/// Installed disposition of one source-local discovery refresh.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryRefreshOutcome {
    /// Metadata binding identifying the refreshed configured source.
    pub metadata_key: crate::AgentMetadataKey,
    /// Exact refresh request represented by the installed context.
    pub refresh_id: u64,
    /// Original setter correlation when the canonical metadata fact carried
    /// one.
    pub mutation_id: Option<crate::AgentMetadataMutationId>,
    /// Explicit degraded outcome, never a claim that the cwd mutation rolled
    /// back.
    pub error: Option<String>,
}

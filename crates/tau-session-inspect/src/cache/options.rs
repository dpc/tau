//! Explicit offline scope and resource policy.

use std::path::PathBuf;

use tau_proto::{AgentId, AgentPromptId, SessionId};

/// Diagnostic projection selected by the offline cache inspector.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CacheView {
    /// Canonical responses, capture coverage, and aggregate evidence.
    #[default]
    Summary,
    /// Provider-reported item attribution and its reconciliation status.
    Attribution,
    /// Attempt, dispatch, anchor, connection, and repair continuity.
    Continuity,
    /// Empirical reported-token distributions within observed regimes.
    Geometry,
    /// Encountered missing, malformed, ambiguous, or bounded evidence.
    Gaps,
}

/// Durable journals selected by an offline cache invocation.
#[derive(Clone)]
pub enum CacheScope {
    /// One agent and, optionally, its authenticated creator descendants.
    Agent {
        /// Root journal identity.
        agent_id: AgentId,
        /// Include recursively creator-owned journals.
        include_descendants: bool,
    },
    /// All agents present in the selected session's membership journal.
    Session(SessionId),
}

/// Inclusive limits for private capture scanning.
#[derive(Clone, serde::Serialize)]
pub struct CacheScanLimits {
    /// Maximum compressed bytes in one capture.
    pub compressed_file_bytes: u64,
    /// Maximum decoded bytes in one capture.
    pub decompressed_file_bytes: u64,
    /// Maximum cumulative decoded capture bytes.
    pub total_decompressed_bytes: u64,
    /// Budget for capture parsing and retained inspection metadata.
    pub working_memory_bytes: u64,
}

impl Default for CacheScanLimits {
    fn default() -> Self {
        Self {
            compressed_file_bytes: 16 * 1024 * 1024,
            decompressed_file_bytes: 64 * 1024 * 1024,
            total_decompressed_bytes: 1024 * 1024 * 1024,
            working_memory_bytes: 512 * 1024 * 1024,
        }
    }
}

/// Inputs to a read-only canonical report and best-effort capture inventory.
pub struct CacheOptions {
    /// Existing state root; this reader never creates it.
    pub state_dir: PathBuf,
    /// Explicit journal selection.
    pub scope: CacheScope,
    /// Optional exact local prompt filter.
    pub prompt: Option<AgentPromptId>,
    /// Requested diagnostic projection.
    pub view: CacheView,
    /// Capture scan admission limits.
    pub limits: CacheScanLimits,
    /// Inspector source/build identity, supplied by its executable.
    pub producer_build: String,
}

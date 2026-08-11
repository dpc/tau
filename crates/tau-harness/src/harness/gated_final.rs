//! Publication-local gated-final continuation state.

use std::collections::BTreeMap;

use tau_proto::{ConnectionId, ProviderResponseFinished};

/// Publication-local post-commit behavior for one gated final candidate.
#[derive(Clone)]
pub(crate) enum GatedFinalDisposition {
    /// Queue the next same-outer-turn reminder after the candidate commits.
    Challenge {
        /// Canonical work title captured with the candidate.
        title: String,
    },
    /// Invalidate Working and perform ordinary or delegated terminal
    /// projection.
    AcceptUnknown {
        /// Complete response-time state needed after the append boundary.
        terminal: Box<CommittedGatedFinal>,
    },
}

/// Response-time state retained until one gated final candidate commits.
#[derive(Clone)]
pub(crate) struct CommittedGatedFinal {
    /// Exact committed provider response.
    pub(super) response: ProviderResponseFinished,
    /// Whether the response carries a compaction projection.
    pub(super) response_contains_compaction: bool,
    /// Prompt input tokens used by context-size alert policy.
    pub(super) input_tokens: Option<u64>,
    /// Captured context-size alert policy for the prompt.
    pub(super) context_size_alerts: BTreeMap<String, tau_config::settings::ContextSizeAlert>,
    /// Whether the response belongs to a non-tool extension query.
    pub(super) is_non_tool_ext_query: bool,
    /// Connection that reported the response.
    pub(super) source: Option<ConnectionId>,
}

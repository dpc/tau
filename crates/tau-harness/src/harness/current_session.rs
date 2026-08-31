//! Current-session counters owned by the harness.

use tau_proto::{TokenCount, TokenUsageStats};

/// Mutable counters and cached usage values scoped to the currently bound
/// session.
#[derive(Debug, Default)]
pub(crate) struct CurrentSessionState {
    /// Input tokens consumed by the most recent agent response, if the provider
    /// reported it. `None` until the first usage report for the current model.
    pub(crate) context_input_tokens: Option<TokenCount>,
    /// Cached input tokens consumed by the most recent agent response, if the
    /// provider reported them.
    pub(crate) context_cached_tokens: Option<TokenCount>,
    /// Percentage of the selected model's context window currently used. `None`
    /// when the model's context window is unknown.
    pub(crate) context_percent_used: Option<u8>,
    /// Current-session token usage totals.
    pub(crate) token_usage: TokenUsageStats,
}

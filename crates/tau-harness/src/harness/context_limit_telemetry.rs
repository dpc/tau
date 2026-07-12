use tau_proto::{ContextLimitObservation, ModelId, PromptOperation};

/// Minimum control-token reserve used by conservative context projection.
pub(super) const MIN_CONTEXT_PROJECTION_RESERVE: u64 = 4096;

/// Immutable content-free context-limit evidence captured at dispatch.
pub(super) struct PromptContextLimitSnapshot {
    /// Exact provider-qualified model.
    pub(super) model: ModelId,
    /// Provider operation dispatched.
    pub(super) operation: PromptOperation,
    /// Conservative input-token projection.
    pub(super) projected_input_tokens: Option<u64>,
    /// Serialized post-baseline transcript growth.
    pub(super) transcript_delta_bytes: u64,
    /// Advertised model window at dispatch.
    pub(super) advertised_context_window: Option<u64>,
    /// Conservative projection reserve.
    pub(super) projection_reserve_tokens: u64,
    /// Explicit role/model compaction threshold at dispatch.
    pub(super) compaction_threshold: Option<u64>,
    /// Sanitized role compaction policy at dispatch.
    pub(super) compaction_policy: tau_proto::ContextLimitCompactionPolicy,
}

/// Returns the shared conservative control-token reserve.
pub(super) fn context_projection_reserve(context_window: u64) -> u64 {
    (context_window / 100).max(MIN_CONTEXT_PROJECTION_RESERVE)
}

/// Classifies sanitized evidence, failing closed for invalid or contradictory
/// provider/projection values.
pub(super) fn context_limit_observation(
    provider_tokens: Option<u64>,
    projected_tokens: Option<u64>,
    advertised_limit: Option<u64>,
) -> ContextLimitObservation {
    let Some(limit) = advertised_limit.filter(|limit| *limit > 0) else {
        return ContextLimitObservation::InsufficientEvidence;
    };
    if provider_tokens == Some(0) {
        return ContextLimitObservation::InsufficientEvidence;
    }
    let provider_side = provider_tokens.map(|tokens| tokens < limit);
    let projection_side = projected_tokens.map(|tokens| tokens < limit);
    if provider_side.is_some() && projection_side.is_some() && provider_side != projection_side {
        return ContextLimitObservation::InsufficientEvidence;
    }
    match provider_tokens.or(projected_tokens) {
        Some(observed) if observed < limit => ContextLimitObservation::RejectedBelowAdvertisedLimit,
        Some(_) => ContextLimitObservation::RejectedAtOrAboveAdvertisedLimit,
        None => ContextLimitObservation::InsufficientEvidence,
    }
}

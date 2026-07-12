use tau_core::AgentEntry;
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
    /// Exact serialized post-baseline transcript growth, when every entry could
    /// be represented as JSON and the total fit in `u64`.
    pub(super) transcript_delta_bytes: Option<u64>,
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

/// Returns the exact JSON byte length used for one transcript-growth entry, or
/// `None` when the entry is not JSON-representable.
pub(super) fn serialized_transcript_entry_bytes(entry: &AgentEntry) -> Option<u64> {
    serde_json::to_vec(entry)
        .ok()
        .and_then(|value| u64::try_from(value.len()).ok())
}

/// Sums exact serialized entry lengths, returning `None` if any entry is not
/// JSON-representable or the exact total overflows.
pub(super) fn serialized_transcript_delta_bytes<'a>(
    entries: impl IntoIterator<Item = &'a AgentEntry>,
) -> Option<u64> {
    entries.into_iter().try_fold(0_u64, |total, entry| {
        total.checked_add(serialized_transcript_entry_bytes(entry)?)
    })
}

/// Derives a projection only when the same-model baseline and exact transcript
/// delta are both available and checked arithmetic succeeds.
pub(super) fn projected_input_tokens(
    baseline: Option<u64>,
    transcript_delta_bytes: Option<u64>,
    reserve: u64,
) -> Option<u64> {
    baseline
        .zip(transcript_delta_bytes)
        .and_then(|(tokens, delta)| tokens.checked_add(delta))
        .and_then(|tokens| tokens.checked_add(reserve))
}

/// Classifies sanitized evidence, failing closed for invalid or contradictory
/// provider/projection values. A conservative byte-derived projection can
/// corroborate or contradict provider usage, but cannot establish a categorical
/// observation without nonzero provider-token evidence.
pub(super) fn context_limit_observation(
    provider_tokens: Option<u64>,
    projected_tokens: Option<u64>,
    advertised_limit: Option<u64>,
) -> ContextLimitObservation {
    let Some(limit) = advertised_limit.filter(|limit| *limit > 0) else {
        return ContextLimitObservation::InsufficientEvidence;
    };
    let Some(provider_tokens) = provider_tokens.filter(|tokens| *tokens > 0) else {
        return ContextLimitObservation::InsufficientEvidence;
    };
    let provider_below_limit = provider_tokens < limit;
    let projection_below_limit = projected_tokens.map(|tokens| tokens < limit);
    if projection_below_limit
        .is_some_and(|projection_below_limit| projection_below_limit != provider_below_limit)
    {
        return ContextLimitObservation::InsufficientEvidence;
    }
    if provider_below_limit {
        ContextLimitObservation::RejectedBelowAdvertisedLimit
    } else {
        ContextLimitObservation::RejectedAtOrAboveAdvertisedLimit
    }
}

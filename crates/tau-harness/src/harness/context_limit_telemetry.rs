use tau_core::AgentEntry;
use tau_proto::{ContextLimitObservation, ModelId, PromptOperation};

/// Immutable content-free context-limit evidence captured at dispatch.
pub(super) struct PromptContextLimitSnapshot {
    /// Exact provider-qualified model.
    pub(super) model: ModelId,
    /// Provider operation dispatched.
    pub(super) operation: PromptOperation,
    /// Exact serialized post-baseline transcript growth, when representable.
    pub(super) transcript_delta_bytes: Option<tau_proto::ByteCount>,
    /// Advertised model token window at dispatch.
    pub(super) advertised_context_window: Option<tau_proto::TokenCount>,
    /// Explicit role/model token threshold at dispatch.
    pub(super) compaction_threshold: Option<tau_proto::TokenCount>,
    /// Sanitized role compaction policy at dispatch.
    pub(super) compaction_policy: tau_proto::ContextLimitCompactionPolicy,
}

/// Exact byte telemetry for a transcript suffix.
#[derive(Clone, Copy)]
pub(super) struct TranscriptGrowth {
    /// Exact JSON-serialized suffix bytes, unavailable on failure.
    pub(super) serialized_bytes: Option<tau_proto::ByteCount>,
}

/// Returns the exact JSON byte length used for one transcript-growth entry.
pub(super) fn serialized_transcript_entry_bytes(
    entry: &AgentEntry,
) -> Option<tau_proto::ByteCount> {
    serde_json::to_vec(entry)
        .ok()
        .and_then(|value| u64::try_from(value.len()).ok())
        .map(tau_proto::ByteCount::new)
}

/// Derives exact byte telemetry from one transcript suffix.
pub(super) fn transcript_growth<'a>(
    entries: impl IntoIterator<Item = &'a AgentEntry>,
) -> TranscriptGrowth {
    TranscriptGrowth {
        serialized_bytes: entries
            .into_iter()
            .try_fold(tau_proto::ByteCount::ZERO, |total, entry| {
                total.checked_add(serialized_transcript_entry_bytes(entry)?)
            }),
    }
}

/// Classifies a canonical rejection using only provider-reported input and the
/// advertised token window.
pub(super) fn context_limit_observation(
    provider_tokens: Option<tau_proto::TokenCount>,
    advertised_limit: Option<tau_proto::TokenCount>,
) -> ContextLimitObservation {
    let Some(limit) = advertised_limit.filter(|limit| *limit > tau_proto::TokenCount::ZERO) else {
        return ContextLimitObservation::InsufficientEvidence;
    };
    let Some(provider_tokens) =
        provider_tokens.filter(|tokens| *tokens > tau_proto::TokenCount::ZERO)
    else {
        return ContextLimitObservation::InsufficientEvidence;
    };
    if provider_tokens < limit {
        ContextLimitObservation::RejectedBelowAdvertisedLimit
    } else {
        ContextLimitObservation::RejectedAtOrAboveAdvertisedLimit
    }
}

use tau_core::AgentEntry;
use tau_proto::{
    ContextItem, ContextLimitObservation, ModelId, PromptOperation, ToolResultContentPart,
};

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

/// Independent exact-byte telemetry and conservative token projection for a
/// transcript suffix.
#[derive(Clone, Copy)]
pub(super) struct TranscriptGrowth {
    /// Exact JSON-serialized suffix bytes, independently unavailable on
    /// failure.
    pub(super) serialized_bytes: Option<u64>,
    /// Conservative suffix token projection, independently unavailable on
    /// failure.
    pub(super) projected_tokens: Option<u64>,
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
#[cfg(test)]
pub(super) fn serialized_transcript_delta_bytes<'a>(
    entries: impl IntoIterator<Item = &'a AgentEntry>,
) -> Option<u64> {
    entries.into_iter().try_fold(0_u64, |total, entry| {
        total.checked_add(serialized_transcript_entry_bytes(entry)?)
    })
}

/// Returns the conservative token projection for one transcript-growth entry.
///
/// Non-image structure retains the existing one-JSON-byte-per-token upper
/// bound. Canonical images are removed from that JSON representation and
/// accounted once by encoded byte length plus one token per 32-by-32 image
/// patch. This avoids the accidental 3-4x expansion from `serde_bytes` JSON
/// integer arrays while keeping provider media visible to proactive compaction
/// accounting.
pub(super) fn projected_transcript_entry_tokens(entry: &AgentEntry) -> Option<u64> {
    let mut metadata_only = entry.clone();
    let image_tokens = strip_and_count_agent_entry_images(&mut metadata_only)?;
    serialized_transcript_entry_bytes(&metadata_only)?.checked_add(image_tokens)
}

/// Derives independent exact-byte telemetry and conservative token projection
/// from one transcript suffix.
pub(super) fn transcript_growth<'a>(
    entries: impl IntoIterator<Item = &'a AgentEntry>,
) -> TranscriptGrowth {
    entries.into_iter().fold(
        TranscriptGrowth {
            serialized_bytes: Some(0),
            projected_tokens: Some(0),
        },
        |growth, entry| TranscriptGrowth {
            serialized_bytes: growth
                .serialized_bytes
                .and_then(|total| total.checked_add(serialized_transcript_entry_bytes(entry)?)),
            projected_tokens: growth
                .projected_tokens
                .and_then(|total| total.checked_add(projected_transcript_entry_tokens(entry)?)),
        },
    )
}

fn strip_and_count_agent_entry_images(entry: &mut AgentEntry) -> Option<u64> {
    let mut total = 0_u64;
    visit_agent_entry_images_mut(entry, |image| {
        let width_patches = u64::from(image.width).div_ceil(32);
        let height_patches = u64::from(image.height).div_ceil(32);
        let patches = width_patches.checked_mul(height_patches)?;
        let image_tokens = u64::try_from(image.data.len()).ok()?.checked_add(patches)?;
        total = total.checked_add(image_tokens)?;
        image.data = std::sync::Arc::from([]);
        Some(())
    })?;
    Some(total)
}

fn visit_agent_entry_images_mut(
    entry: &mut AgentEntry,
    mut visit: impl FnMut(&mut tau_proto::ImageContent) -> Option<()>,
) -> Option<()> {
    let mut visit_items = |items: &mut [ContextItem]| {
        for item in items {
            if let ContextItem::ToolResult(result) = item {
                for part in &mut result.provider_content {
                    let ToolResultContentPart::Image(image) = part;
                    visit(image)?;
                }
            }
        }
        Some(())
    };
    match entry {
        AgentEntry::UserInput { items, .. }
        | AgentEntry::AssistantResponse {
            output_items: items,
            ..
        }
        | AgentEntry::Compaction {
            replacement_window: items,
            ..
        } => visit_items(items),
        AgentEntry::ToolResults { items } => {
            for result in items {
                for part in &mut result.provider_content {
                    let ToolResultContentPart::Image(image) = part;
                    visit(image)?;
                }
            }
            Some(())
        }
        AgentEntry::AgentMessage { .. }
        | AgentEntry::MessageFact { .. }
        | AgentEntry::CompactionTrigger { .. } => Some(()),
    }
}

/// Derives a projection only when the same-model baseline and exact transcript
/// token delta are both available and checked arithmetic succeeds.
pub(super) fn projected_input_tokens(
    baseline: Option<u64>,
    transcript_delta_tokens: Option<u64>,
    reserve: u64,
) -> Option<u64> {
    baseline
        .zip(transcript_delta_tokens)
        .and_then(|(tokens, delta)| tokens.checked_add(delta))
        .and_then(|tokens| tokens.checked_add(reserve))
}

/// Classifies sanitized evidence, failing closed for invalid or contradictory
/// provider/projection values. A conservative transcript projection can
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

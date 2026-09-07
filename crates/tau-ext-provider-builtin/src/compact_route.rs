//! Private native-route evidence and one-shot local-summary selection.

use std::collections::{HashMap, HashSet};

use tau_proto::{ProviderModelInfo, ProviderName};
use tau_provider_codex::{CompactOutcome, InferenceProfileIdentity};

/// Switch implementation once, only after definitive native absence. Neither
/// cancellation nor post-progress failure can produce this typed outcome.
pub(super) fn compact_with_local_fallback(
    outcome: CompactOutcome,
    mut unavailable: impl FnMut(InferenceProfileIdentity),
    local: impl FnOnce() -> CompactOutcome,
) -> CompactOutcome {
    if let CompactOutcome::RouteUnavailable {
        newly_downgraded,
        profile_identity,
        backend_reached: native_reached,
        ..
    } = outcome
    {
        if newly_downgraded {
            unavailable(profile_identity);
        }
        let mut outcome = local();
        match &mut outcome {
            CompactOutcome::Finished { .. } => {}
            CompactOutcome::Retry {
                backend_reached, ..
            }
            | CompactOutcome::Canceled { backend_reached }
            | CompactOutcome::Terminal {
                backend_reached, ..
            }
            | CompactOutcome::RouteUnavailable {
                backend_reached, ..
            } => {
                *backend_reached |= native_reached;
            }
        }
        outcome
    } else {
        outcome
    }
}

/// Native absence removes only the native provider-default boundary: the
/// adapter still owns a local-summary lowerer for the common operation.
pub(super) fn apply_compact_route_downgrades(
    models: &mut [ProviderModelInfo],
    identities: &HashMap<ProviderName, InferenceProfileIdentity>,
    unavailable: &HashSet<InferenceProfileIdentity>,
) {
    for model in models {
        if identities
            .get(&model.id.provider)
            .is_some_and(|identity| unavailable.contains(identity))
            && model.supports_standalone_compaction
        {
            model.standalone_compaction_threshold = None;
        }
    }
}

//! A paste owns the unchanged editor until its upload finishes or is discarded.

use std::sync::Arc;

#[cfg(test)]
mod tests;

/// Source text retained outside prompt, history, completion, and draft
/// reporting.
pub(super) struct PendingPaste {
    /// Unique attempt identity; stale completions cannot edit a later draft.
    pub(super) id: u64,
    /// Normalized UTF-8 source retained for explicit retry after failure.
    pub(super) text: Arc<str>,
    /// Only a failed attempt accepts Enter as an explicit retry.
    pub(super) failed: bool,
}

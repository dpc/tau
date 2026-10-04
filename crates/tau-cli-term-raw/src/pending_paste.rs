//! A paste owns the unchanged editor until its upload finishes or is discarded.

#[cfg(test)]
mod tests;

/// Source text retained outside prompt, history, completion, and draft
/// reporting.
pub(super) struct PendingPaste {
    /// Unique attempt identity; stale completions cannot edit a later draft.
    pub(super) id: u64,
    /// Completed source retained for explicit artifact-upload retry after
    /// failure.
    pub(super) content: crate::PasteContent,
    /// Only a failed attempt accepts Enter as an explicit retry.
    pub(super) failed: bool,
}

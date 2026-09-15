//! Original-event shape validation for native Codex standalone compaction.

use crate::common::LlmError;

const INVALID_COMPACT_SHAPE: &str =
    "compaction response did not contain exactly one completed compaction item";

/// Progress through the compact-only provider event language.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CompactItemPhase {
    /// No output slot has appeared.
    #[default]
    Missing,
    /// Slot zero contains an added, but not completed, compaction item.
    Added,
    /// Slot zero contains the one completed compaction item.
    Done,
}

/// Validates native compact shape from original provider events before
/// projection.
#[derive(Debug, Default)]
pub(super) struct CompactStreamShape {
    /// Current output-slot progress.
    item: CompactItemPhase,
    /// Identity observed on the optional added event.
    added_item_id: Option<Option<String>>,
    /// Validated activity notifications, independent of provider sequence
    /// numbers.
    pub(super) progress_updates: u64,
}

impl CompactStreamShape {
    /// Validates one original provider event in arrival order.
    pub(super) fn validate(&mut self, event: &serde_json::Value) -> Result<(), LlmError> {
        let event_type = event["type"].as_str().unwrap_or("");
        match event_type {
            "response.compaction.compacting" => {
                if self.item != CompactItemPhase::Added
                    || event["output_index"].as_u64() != Some(0)
                    || event["item_id"].as_str().is_none()
                    || self.added_item_id.as_ref().and_then(|id| id.as_deref())
                        != event["item_id"].as_str()
                {
                    return self.reject("invalid_compacting_notification");
                }
                self.progress_updates = self.progress_updates.saturating_add(1);
                Ok(())
            }
            "response.output_item.added" => {
                self.validate_compaction_item(event, CompactItemPhase::Added)
            }
            "response.output_item.done" => {
                self.validate_compaction_item(event, CompactItemPhase::Done)
            }
            "response.completed" if self.item == CompactItemPhase::Done => Ok(()),
            "response.completed" | "response.done" => {
                self.reject("terminal_before_completed_item_or_unsupported_done")
            }
            "response.created" | "response.in_progress"
                if self.item == CompactItemPhase::Missing =>
            {
                Ok(())
            }
            "codex.rate_limits" => Ok(()),
            "response.incomplete" | "response.failed" | "error" => Ok(()),
            event_type if event_type.starts_with("response.") => {
                self.reject("unexpected_response_event")
            }
            _ => Ok(()),
        }
    }

    /// Validates the sole output slot without projecting or retaining its
    /// payload.
    fn validate_compaction_item(
        &mut self,
        event: &serde_json::Value,
        next: CompactItemPhase,
    ) -> Result<(), LlmError> {
        if event["output_index"].as_u64() != Some(0)
            || event["item"]["type"].as_str() != Some("compaction")
        {
            return self.reject("expected_compaction_item_at_output_index_zero");
        }
        let valid_transition = matches!(
            (self.item, next),
            (
                CompactItemPhase::Missing,
                CompactItemPhase::Added | CompactItemPhase::Done
            ) | (CompactItemPhase::Added, CompactItemPhase::Done)
        );
        if !valid_transition {
            return self.reject("invalid_item_phase_transition");
        }
        let item_id = event["item"]["id"].as_str().map(str::to_owned);
        if next == CompactItemPhase::Added {
            self.added_item_id = Some(item_id);
        } else if self.item == CompactItemPhase::Added
            && self.added_item_id.as_ref() != Some(&item_id)
        {
            return self.reject("added_done_item_identity_mismatch");
        }
        self.item = next;
        Ok(())
    }

    /// Preserve the exact local rejection branch without logging provider
    /// payloads.
    fn reject(&self, reason: &'static str) -> Result<(), LlmError> {
        Err(LlmError::InvalidResponse(format!(
            "{INVALID_COMPACT_SHAPE}: {reason} (phase={:?})",
            self.item
        )))
    }
}

#[cfg(test)]
mod tests;

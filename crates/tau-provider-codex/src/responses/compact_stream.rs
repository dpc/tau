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
}

impl CompactStreamShape {
    /// Validates one original provider event in arrival order.
    pub(super) fn validate(&mut self, event: &serde_json::Value) -> Result<(), LlmError> {
        let event_type = event["type"].as_str().unwrap_or("");
        match event_type {
            "response.output_item.added" => {
                self.validate_compaction_item(event, CompactItemPhase::Added)
            }
            "response.output_item.done" => {
                self.validate_compaction_item(event, CompactItemPhase::Done)
            }
            "response.completed" if self.item == CompactItemPhase::Done => Ok(()),
            "response.completed" | "response.done" => Err(invalid_compact_shape()),
            "response.created" | "response.in_progress"
                if self.item == CompactItemPhase::Missing =>
            {
                Ok(())
            }
            "codex.rate_limits" => Ok(()),
            "response.incomplete" | "response.failed" | "error" => Ok(()),
            event_type if event_type.starts_with("response.") => Err(invalid_compact_shape()),
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
            return Err(invalid_compact_shape());
        }
        let valid_transition = matches!(
            (self.item, next),
            (
                CompactItemPhase::Missing,
                CompactItemPhase::Added | CompactItemPhase::Done
            ) | (CompactItemPhase::Added, CompactItemPhase::Done)
        );
        if !valid_transition {
            return Err(invalid_compact_shape());
        }
        let item_id = event["item"]["id"].as_str().map(str::to_owned);
        if next == CompactItemPhase::Added {
            self.added_item_id = Some(item_id);
        } else if self.item == CompactItemPhase::Added
            && self.added_item_id.as_ref() != Some(&item_id)
        {
            return Err(invalid_compact_shape());
        }
        self.item = next;
        Ok(())
    }
}

/// Constructs the fixed compact-shape validation failure.
fn invalid_compact_shape() -> LlmError {
    LlmError::InvalidResponse(INVALID_COMPACT_SHAPE.to_owned())
}

#[cfg(test)]
mod tests;

//! Original-event shape validation for native Codex standalone compaction.

use crate::common::LlmError;

mod unknown_diagnostics;

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
    /// Per-attempt bounded compatibility observations, never output state.
    unknown: unknown_diagnostics::UnknownDiagnostics,
}

impl CompactStreamShape {
    /// Validates live input and reports unknown presence without logging
    /// payloads.
    pub(super) fn validate_observed(
        &mut self,
        event: &serde_json::Value,
        prompt: &str,
    ) -> Result<(), LlmError> {
        let kind = event["type"].as_str().unwrap_or("");
        if matches!(
            kind,
            "response.output_item.added" | "response.output_item.done"
        ) {
            if event["item"]["type"].as_str() != Some("compaction") {
                self.unknown.observe(
                    prompt,
                    self.item,
                    "output_item_type",
                    event["item"]["type"].as_str(),
                    "rejected_output_item",
                );
            }
        } else if !matches!(
            kind,
            "response.compaction.compacting"
                | "response.created"
                | "response.in_progress"
                | "response.completed"
                | "response.done"
                | "response.incomplete"
                | "response.failed"
                | "error"
                | "codex.rate_limits"
                | "codex.response.metadata"
        ) {
            let disposition = if self.is_ignored_notification(event) {
                "ignored_informational"
            } else {
                "rejected_unknown_shape"
            };
            self.unknown.observe(
                prompt,
                self.item,
                "event_type",
                event["type"].as_str(),
                disposition,
            );
        }
        self.validate(event)
    }

    /// Only notification namespaces with identity/ordering-only fields are safe
    /// to discard. Unknown output and ambiguous semantic families remain
    /// errors.
    pub(super) fn is_ignored_notification(&self, event: &serde_json::Value) -> bool {
        let Some(fields) = event.as_object() else {
            return false;
        };
        let Some(kind) = event["type"].as_str() else {
            return false;
        };
        if unknown_diagnostics::safe_label(Some(kind)) != kind
            || !(kind.starts_with("response.compaction.")
                || kind.starts_with("response.notification.")
                || kind.starts_with("codex.notification."))
            || kind == "response.compaction.compacting"
            || has_lifecycle_suffix(kind)
            || fields.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type" | "sequence_number" | "output_index" | "item_id"
                )
            })
            || fields
                .get("sequence_number")
                .is_some_and(|n| n.as_u64().is_none())
        {
            return false;
        }
        match (fields.get("output_index"), fields.get("item_id")) {
            (None, None) => true,
            (Some(index), Some(id)) => {
                self.item == CompactItemPhase::Added
                    && index.as_u64() == Some(0)
                    && id.as_str().is_some()
                    && self.added_item_id.as_ref().and_then(|id| id.as_deref()) == id.as_str()
            }
            _ => false,
        }
    }

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
            "codex.rate_limits" | "codex.response.metadata" => Ok(()),
            "response.incomplete" | "response.failed" | "error" => Ok(()),
            _ if self.is_ignored_notification(event) => Ok(()),
            _ => self.reject("unexpected_response_event"),
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

/// Unknown lifecycle/control or output-delta events cannot be discarded merely
/// because their object carries only identity fields.
fn has_lifecycle_suffix(kind: &str) -> bool {
    matches!(
        kind.rsplit('.').next(),
        Some(
            "added"
                | "done"
                | "delta"
                | "created"
                | "in_progress"
                | "started"
                | "completed"
                | "failed"
                | "incomplete"
                | "error"
                | "canceled"
                | "cancelled"
        )
    )
}

#[cfg(test)]
mod tests;

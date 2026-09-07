//! Provisional local-summary replay preserves each response and control
//! boundary.

use serde::{Deserialize, Serialize};

use crate::{AssistantResponseBlock, UserInputBlock};

/// Exact harness-owned instruction for appending to an unfinished summary.
pub const LOCAL_SUMMARY_CONTINUATION_INSTRUCTION: &str = "The previous summary response reached the output-token limit. Continue the same summary from exactly where it stopped, using the retained reasoning. Your summary text will be appended directly to the existing draft: do not repeat or rewrite earlier summary text. If no summary text has been written yet, begin the summary now. Finish the summary without extending the analysis. Do not continue the original task or make or request tool calls.";

/// One provisional response followed by its harness-authored continuation
/// steer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalSummaryContinuationStep {
    /// Exactly one prior attempt, retaining native reasoning/prose grouping.
    pub response: AssistantResponseBlock,
    /// Shared-envelope instruction assembled by the harness, not the adapter.
    pub steer: UserInputBlock,
}

/// Assemble the fixed harness-authored steer through the registered family.
#[must_use]
pub fn local_summary_continuation_steer() -> UserInputBlock {
    let envelope = crate::TAU_INTERNAL_PAYLOAD_ENVELOPE;
    let crate::PayloadEnvelopeOpening::Fixed(open) = envelope.opening else {
        unreachable!("internal payload has a fixed opening");
    };
    UserInputBlock {
        items: vec![crate::ContextItem::Message(crate::MessageItem {
            role: crate::ContextRole::User,
            content: vec![crate::ContentPart::Text {
                text: format!(
                    "{open}\n{}\n{}",
                    envelope.escape_body(LOCAL_SUMMARY_CONTINUATION_INSTRUCTION),
                    envelope.exact_close,
                ),
            }],
            phase: None,
            responses_raw_json: None,
        })],
    }
}

/// Validate replayable local-summary output and return its exact narrative and
/// separately counted full reasoning bytes. Empty output is structurally valid
/// but cannot authorize an output-limit continuation.
///
/// # Errors
///
/// Rejects unsupported output or either channel exceeding the shared ceiling.
pub fn local_summary_output_parts(
    items: &[crate::ContextItem],
) -> Result<(String, usize), &'static str> {
    let mut text = String::new();
    let mut reasoning_bytes = 0_usize;
    let mut message_seen = false;
    for item in items {
        match item {
            crate::ContextItem::Message(message)
                if !message_seen
                    && message.role == crate::ContextRole::Assistant
                    && message.responses_raw_json.is_none() =>
            {
                message_seen = true;
                for part in &message.content {
                    let crate::ContentPart::Text { text: fragment } = part else {
                        return Err("summary continuation contains unsupported message content");
                    };
                    if fragment.len() > crate::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES - text.len() {
                        return Err("summary narrative exceeds its byte limit");
                    }
                    text.push_str(fragment);
                }
            }
            crate::ContextItem::ReasoningText(reasoning)
                if reasoning.kind == crate::ReasoningTextKind::Full =>
            {
                reasoning_bytes = reasoning_bytes
                    .checked_add(reasoning.text.len())
                    .filter(|bytes| *bytes <= crate::LOCAL_COMPACTION_NARRATIVE_MAX_BYTES)
                    .ok_or("summary reasoning exceeds its byte limit")?;
            }
            _ => return Err("summary continuation contains unsupported output"),
        }
    }
    Ok((text, reasoning_bytes))
}

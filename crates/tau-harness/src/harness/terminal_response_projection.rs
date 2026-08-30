//! One-pass projection of provider terminal output.

use super::*;

#[cfg(test)]
thread_local! {
    /// Production-pass counter used by discard-path work oracles.
    static PROJECTION_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Response-output facts shared by terminal classification and reduction.
///
/// The projection scans provider output once. It owns only the facts that must
/// survive later in-place normalization; the canonical output remains owned by
/// the response.
pub(super) struct TerminalResponseProjection {
    /// Provider tool calls in declaration order.
    pub(super) tool_calls: Vec<AgentToolCall>,
    /// Contiguous assistant text needed by continuation reducers.
    pub(super) assistant_text: Option<String>,
    /// Whether any output item carries a compaction.
    pub(super) contains_compaction: bool,
    /// Whether output contains standalone-private compaction material.
    pub(super) contains_private_compaction_output: bool,
}

impl TerminalResponseProjection {
    /// Project all shared terminal-output facts in one traversal.
    pub(super) fn from_response(response: &ProviderResponseFinished) -> Self {
        Self::from_output_items(&response.output_items)
    }

    /// Project shared facts from one canonical terminal output slice.
    pub(super) fn from_output_items(output_items: &[ContextItem]) -> Self {
        #[cfg(test)]
        PROJECTION_PASSES.with(|passes| passes.set(passes.get().saturating_add(1)));
        let mut tool_calls = Vec::new();
        let mut assistant_text = String::new();
        let mut contains_compaction = false;
        let mut contains_private_compaction_output = false;

        for item in output_items {
            match item {
                ContextItem::ToolCall(call) => tool_calls.push(AgentToolCall {
                    call_ref: None,
                    id: call.call_id.clone(),
                    name: call.name.clone(),
                    tool_type: call.tool_type,
                    arguments: call.arguments.clone(),
                }),
                ContextItem::Message(MessageItem {
                    role: ContextRole::Assistant,
                    content,
                    ..
                }) => {
                    for part in content {
                        match part {
                            ContentPart::Text { text }
                            | ContentPart::SyntheticCompactionSummary { text }
                            | ContentPart::HarnessInternalText { text } => {
                                assistant_text.push_str(text);
                            }
                        }
                        contains_private_compaction_output |=
                            matches!(part, ContentPart::SyntheticCompactionSummary { .. });
                    }
                }
                ContextItem::Message(message) => {
                    contains_private_compaction_output |= message
                        .content
                        .iter()
                        .any(|part| matches!(part, ContentPart::SyntheticCompactionSummary { .. }));
                }
                ContextItem::Compaction(_) => contains_compaction = true,
                ContextItem::LocalCompactionNarrative(_) => {
                    contains_private_compaction_output = true;
                }
                _ => {}
            }
        }

        Self {
            tool_calls,
            assistant_text: (!assistant_text.is_empty()).then_some(assistant_text),
            contains_compaction,
            contains_private_compaction_output,
        }
    }

    /// Forget facts removed by malformed-repetition normalization.
    pub(super) fn clear_output(&mut self) {
        self.tool_calls.clear();
        self.assistant_text = None;
        self.contains_compaction = false;
        self.contains_private_compaction_output = false;
    }
}

/// Reset and return the number of production projection passes on this test
/// thread.
#[cfg(test)]
pub(super) fn take_projection_passes() -> usize {
    PROJECTION_PASSES.with(|passes| passes.replace(0))
}

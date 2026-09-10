//! Destination-aware projection under `REQ-best-effort-provider-switching`.

use tau_proto::{ContextItem, ProviderName};

/// Request-local conversion state; canonical transcript items remain untouched.
pub(super) struct ProviderContextProjection<'a> {
    /// Configured destination provider, independent of the model name.
    pub(super) destination: &'a ProviderName,
    /// Whether this projection omitted any incompatible replay material.
    pub(super) omitted: bool,
    /// Whether conversion would discard an opaque history replacement.
    pub(super) incompatible_compaction: bool,
}

impl ProviderContextProjection<'_> {
    /// Removes only provider-owned material lacking known compatible origin.
    pub(super) fn project(
        &mut self,
        tree: &tau_core::AgentTree,
        node: Option<tau_core::NodeId>,
        block: &mut tau_proto::ContextBlock,
        replaces_history: bool,
    ) {
        let compatible = node
            .and_then(|node| tree.provider_model_for_node(node))
            .is_some_and(|model| &model.provider == self.destination);
        if compatible {
            return;
        }
        let items = match block {
            tau_proto::ContextBlock::UserInput(input) => &mut input.items,
            tau_proto::ContextBlock::AssistantResponse(response) => {
                self.omitted |= response.provider_response_id.take().is_some();
                &mut response.output_items
            }
            tau_proto::ContextBlock::ToolResults(_) => return,
        };
        items.retain_mut(|item| match item {
            ContextItem::Reasoning(_)
            | ContextItem::ReasoningText(_)
            | ContextItem::UnknownProviderItem(_) => {
                self.omitted = true;
                false
            }
            ContextItem::Compaction(_) => {
                self.incompatible_compaction |= replaces_history;
                self.omitted = true;
                false
            }
            ContextItem::Message(message) => {
                self.omitted |= message.responses_raw_json.take().is_some();
                true
            }
            ContextItem::ToolCall(call) => {
                self.omitted |= call.responses_envelope.take().is_some();
                true
            }
            ContextItem::ToolResult(_)
            | ContextItem::LocalCompactionNarrative(_)
            | ContextItem::CompactionTrigger => true,
        });
    }
}

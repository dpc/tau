//! ChatGPT Responses-v2 standalone compaction replacement projection.

use tau_proto::{ContentPart, ContextBlock, ContextItem, ContextRole, MessageItem, PromptContext};

const RETAINED_MESSAGE_BYTE_BUDGET: usize = 256_000;
const MAX_RETAINED_AGENT_MESSAGE_BYTES: usize = 40_000;

#[cfg(test)]
thread_local! {
    /// Counts message clones performed by the production replacement builder.
    static RETAINED_MESSAGE_CLONES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Builds the approved ChatGPT-v2 replacement from exact retained input and
/// the single canonical provider compaction item.
pub(super) fn build_v2_compacted_window(
    context: &PromptContext,
    mut provider_output: Vec<ContextItem>,
) -> Vec<ContextItem> {
    let candidates = context
        .blocks
        .iter()
        .rev()
        .filter_map(|block| match block {
            ContextBlock::UserInput(block) => Some(block.items.as_slice()),
            ContextBlock::AssistantResponse(block) => Some(block.output_items.as_slice()),
            ContextBlock::ToolResults(_) => None,
        })
        .flat_map(|items| items.iter().rev())
        .filter_map(retained_message);
    let mut remaining = RETAINED_MESSAGE_BYTE_BUDGET;
    let mut retained = Vec::new();
    for item in candidates {
        if remaining == 0 {
            break;
        }
        let bytes = message_bytes(item);
        if bytes <= remaining {
            remaining = remaining.saturating_sub(bytes);
            retained.push(ContextItem::Message(clone_retained_message(item)));
        } else if let Some(item) = truncate_message(clone_retained_message(item), remaining) {
            retained.push(ContextItem::Message(item));
            break;
        } else {
            break;
        }
    }
    retained.reverse();
    retained.append(&mut provider_output);
    retained
}

/// Borrows one eligible retained message without materializing a candidate.
fn retained_message(item: &ContextItem) -> Option<&MessageItem> {
    let ContextItem::Message(message) = item else {
        return None;
    };
    if message.role != ContextRole::User {
        return None;
    }
    let first_text = message.content.first().and_then(content_text);
    if first_text.is_some_and(|text| {
        text.starts_with("<tau_internal>") && text.contains(" emitted a response\n\n<response>")
    }) {
        return None;
    }
    let is_agent_message = message
        .content
        .first()
        .and_then(content_text)
        .is_some_and(is_non_final_agent_message);
    if is_agent_message && MAX_RETAINED_AGENT_MESSAGE_BYTES < message_bytes(message) {
        return None;
    }
    Some(message)
}

/// Clones one message only after newest-first aggregate admission reaches it.
fn clone_retained_message(message: &MessageItem) -> MessageItem {
    #[cfg(test)]
    RETAINED_MESSAGE_CLONES.with(|count| count.set(count.get().saturating_add(1)));
    message.clone()
}

fn is_non_final_agent_message(text: &str) -> bool {
    (text.starts_with("<tau_internal>") && text.contains("\n\n<message>"))
        || text.starts_with("<tau_peer_message ")
        || (text.starts_with("<tau_internal>") && text.contains("\n\n<tau_peer_message "))
        || (text.starts_with("<tau_internal>Watched agent ")
            && text.contains(" received a user prompt\n\n<prompt>\n")
            && text.ends_with("\n</prompt>&lt;/tau_internal&gt;"))
}

fn message_bytes(message: &MessageItem) -> usize {
    message
        .content
        .iter()
        .filter_map(content_text)
        .map(str::len)
        .sum()
}

fn content_text(part: &ContentPart) -> Option<&str> {
    match part {
        ContentPart::Text { text }
        | ContentPart::SyntheticCompactionSummary { text }
        | ContentPart::HarnessInternalText { text } => Some(text),
        ContentPart::UrlCitation { .. } | ContentPart::CitationMetadataInvalid => None,
    }
}

fn truncate_message(mut message: MessageItem, max_bytes: usize) -> Option<MessageItem> {
    let mut remaining = max_bytes;
    let mut content = Vec::new();
    for mut part in message.content {
        let Some(text) = (match &mut part {
            ContentPart::Text { text }
            | ContentPart::SyntheticCompactionSummary { text }
            | ContentPart::HarnessInternalText { text } => Some(text),
            ContentPart::UrlCitation { .. } | ContentPart::CitationMetadataInvalid => None,
        }) else {
            content.push(part);
            continue;
        };
        if remaining == 0 {
            continue;
        }
        let bytes = text.len();
        if bytes <= remaining {
            remaining = remaining.saturating_sub(bytes);
        } else {
            *text = truncate_middle(text, remaining);
            remaining = 0;
        }
        if !text.is_empty() {
            content.push(part);
        }
    }
    if content.is_empty() {
        return None;
    }
    message.content = content;
    Some(message)
}

fn truncate_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let marker_reserve = "…18446744073709551615 bytes truncated…".len();
    if max_bytes <= marker_reserve {
        return String::new();
    }
    let content_budget = max_bytes.saturating_sub(marker_reserve);
    let left_target = content_budget / 2;
    let right_target = content_budget.saturating_sub(left_target);
    let left = floor_char_boundary(text, left_target);
    let right_start = ceil_char_boundary(text, text.len().saturating_sub(right_target));
    let removed_bytes = text[left..right_start].len();
    format!(
        "{}…{removed_bytes} bytes truncated…{}",
        &text[..left],
        &text[right_start..]
    )
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index = index.saturating_sub(1);
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests;

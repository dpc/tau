//! ChatGPT Responses-v2 standalone compaction replacement projection.

use tau_proto::{ContentPart, ContextItem, ContextRole, MessageItem, PromptContext};

const RETAINED_MESSAGE_TOKEN_BUDGET: usize = 64_000;
const MAX_RETAINED_AGENT_MESSAGE_TOKENS: usize = 10_000;

/// Builds the approved ChatGPT-v2 replacement from exact retained input and
/// the single canonical provider compaction item.
pub(super) fn build_v2_compacted_window(
    context: &PromptContext,
    mut provider_output: Vec<ContextItem>,
) -> Vec<ContextItem> {
    let candidates = context
        .flatten_iter()
        .filter_map(|item| retained_message(&item))
        .collect::<Vec<_>>();
    let mut remaining = RETAINED_MESSAGE_TOKEN_BUDGET;
    let mut retained = Vec::new();
    for item in candidates.into_iter().rev() {
        if remaining == 0 {
            continue;
        }
        let tokens = message_tokens(&item).max(1);
        if tokens <= remaining {
            remaining = remaining.saturating_sub(tokens);
            retained.push(ContextItem::Message(item));
        } else if let Some(item) = truncate_message(item, remaining) {
            retained.push(ContextItem::Message(item));
            remaining = 0;
        }
    }
    retained.reverse();
    retained.append(&mut provider_output);
    retained
}

fn retained_message(item: &ContextItem) -> Option<MessageItem> {
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
    let is_agent_message = message.content.first().is_some_and(|part| {
        content_text(part).is_some_and(|text| {
            (text.starts_with("<tau_internal>") && text.contains("\n\n<message>"))
                || text.starts_with("<tau_peer_message ")
                || (text.starts_with("<tau_internal>") && text.contains("\n\n<tau_peer_message "))
        })
    });
    if is_agent_message && MAX_RETAINED_AGENT_MESSAGE_TOKENS < message_tokens(message) {
        return None;
    }
    Some(message.clone())
}

fn message_tokens(message: &MessageItem) -> usize {
    message
        .content
        .iter()
        .filter_map(content_text)
        .map(|text| text.len().div_ceil(4))
        .sum()
}

fn content_text(part: &ContentPart) -> Option<&str> {
    match part {
        ContentPart::Text { text } | ContentPart::HarnessInternalText { text } => Some(text),
    }
}

fn truncate_message(mut message: MessageItem, max_tokens: usize) -> Option<MessageItem> {
    let mut remaining = max_tokens;
    let mut content = Vec::new();
    for mut part in message.content {
        let text = match &mut part {
            ContentPart::Text { text } | ContentPart::HarnessInternalText { text } => text,
        };
        if remaining == 0 {
            continue;
        }
        let tokens = text.len().div_ceil(4);
        if tokens <= remaining {
            remaining = remaining.saturating_sub(tokens);
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

fn truncate_middle(text: &str, max_tokens: usize) -> String {
    let max_bytes = max_tokens.saturating_mul(4);
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let marker_reserve = "…18446744073709551615 tokens truncated…".len();
    if max_bytes <= marker_reserve {
        return String::new();
    }
    let content_budget = max_bytes.saturating_sub(marker_reserve);
    let left_target = content_budget / 2;
    let right_target = content_budget.saturating_sub(left_target);
    let left = floor_char_boundary(text, left_target);
    let right_start = ceil_char_boundary(text, text.len().saturating_sub(right_target));
    let removed_tokens = text[left..right_start].len().div_ceil(4);
    format!(
        "{}…{removed_tokens} tokens truncated…{}",
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

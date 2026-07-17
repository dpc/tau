//! Policy-safe normalization of the authenticated Slack transport identity.

/// Model-visible semantic reference to this Slack transport instance.
pub(super) const SLACK_BRIDGE_REFERENCE: &str = "@slack_bridge";

/// Normalized Slack text plus leading-mention command compatibility.
pub(super) struct NormalizedTransportMention {
    /// Canonical logical/model text with eligible own mentions normalized.
    pub(super) text: String,
    /// Whether exactly one leading eligible mention was removed.
    pub(super) leading: bool,
}

/// Normalize exact own-bot entities outside complete backtick code ranges.
///
/// Input is trimmed but not HTML-decoded. Exactly one eligible leading entity
/// and its following whitespace are removed for command compatibility. Every
/// remaining eligible entity becomes [`SLACK_BRIDGE_REFERENCE`]. Complete
/// equal-length backtick-delimited ranges remain byte-for-byte unchanged;
/// unmatched backticks are literal and suppress nothing.
pub(super) fn normalize_transport_mentions(
    text: &str,
    bot_user_id: &str,
) -> NormalizedTransportMention {
    let text = text.trim();
    let native = format!("<@{bot_user_id}>");
    let code_ranges = complete_backtick_ranges(text);
    let leading =
        text.starts_with(&native) && code_ranges.first().is_none_or(|(start, _)| *start != 0);
    let start = if leading {
        let remainder = &text[native.len()..];
        native.len() + (remainder.len() - remainder.trim_start().len())
    } else {
        0
    };
    let mut output = String::with_capacity(text.len());
    let mut index = start;
    let mut range_index = code_ranges.partition_point(|(_, end)| *end <= index);
    while index < text.len() {
        if let Some((range_start, range_end)) = code_ranges.get(range_index).copied()
            && index == range_start
        {
            output.push_str(&text[range_start..range_end]);
            index = range_end;
            range_index += 1;
            continue;
        }
        if text[index..].starts_with(&native) {
            output.push_str(SLACK_BRIDGE_REFERENCE);
            index += native.len();
            continue;
        }
        let character = text[index..].chars().next().expect("valid text index");
        output.push(character);
        index += character.len_utf8();
    }
    NormalizedTransportMention {
        text: output.trim().to_owned(),
        leading,
    }
}

/// Return byte ranges covered by complete equal-length backtick delimiters.
fn complete_backtick_ranges(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut open = 0;
    while open < bytes.len() {
        let Some(relative) = bytes[open..].iter().position(|byte| *byte == b'`') else {
            break;
        };
        open += relative;
        let open_end = backtick_run_end(bytes, open);
        let delimiter_len = open_end - open;
        let mut cursor = open_end;
        let mut close = None;
        while cursor < bytes.len() {
            let Some(relative) = bytes[cursor..].iter().position(|byte| *byte == b'`') else {
                break;
            };
            let candidate = cursor + relative;
            let candidate_end = backtick_run_end(bytes, candidate);
            if candidate_end - candidate == delimiter_len {
                close = Some(candidate_end);
                break;
            }
            cursor = candidate_end;
        }
        if let Some(close_end) = close {
            ranges.push((open, close_end));
            open = close_end;
        } else {
            open = open_end;
        }
    }
    ranges
}

/// Return the first byte after one ASCII backtick run.
fn backtick_run_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while bytes.get(end) == Some(&b'`') {
        end += 1;
    }
    end
}
